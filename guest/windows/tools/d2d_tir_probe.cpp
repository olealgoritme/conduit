// d2d_tir_probe.cpp: Direct2D anti-aliased fills through target-independent
// rasterization (TIR), compared against WARP.
//
// Direct2D fills complex geometry (the Windows 11 shell's vector icons, for
// example) with D3D11.1 TIR: a rasterizer state with ForcedSampleCount 16, a
// single-sample render target, and a pixel shader that takes its alpha from
// the number of covered samples in SV_Coverage. A driver that rasterizes those
// draws at the render target's single sample gives every covered pixel 1/16 of
// its coverage: the fill comes out almost transparent (maximum alpha 16).
//
// First, without Direct2D, the probe draws one triangle with a
// ForcedSampleCount 16 rasterizer state into a single-sample target, with a
// pixel shader that writes countbits(SV_Coverage) / 16: full alpha inside,
// and all 15 partial levels along the slanted edge. Then, each printing
// PASS or FAIL:
//   - ForcedSampleCount 8 and 4 (the other raster to color sample ratios);
//   - UAV-only rendering (no render target) with ForcedSampleCount 16, the
//     pixel shader storing the covered sample count into a UAV (a device
//     that rasterizes at most 8 samples without attachments shows 8);
//   - ForcedSampleCount 16 with a depth-stencil view bound and depth test
//     off, plus a depth clear in between (undefined in D3D11: the device
//     must survive it);
//   - EvaluateAttributeAtSample at sample indices 8 to 15 of the forced
//     16-sample pattern, compared with the standard pattern.
//
// Then it hooks ID3D11Device1::CreateRasterizerState1 to print the forced
// sample counts Direct2D asks for, and fills a few shapes white on
// transparent black into an offscreen BGRA texture and prints, per shape, the
// maximum alpha, the pixels at full alpha and the pixels covered at all.
// Simple shapes take Direct2D's tessellated path; on a hardware device the
// 400-point star and the Bezier ring take TIR with ForcedSampleCount 16 (WARP
// is never asked for more than 1). A correct driver prints max alpha 255 for
// every row, and pixel counts close to WARP's.
//
// Build (host):
//   x86_64-w64-mingw32-g++ -Os -s -static -o d2d_tir_probe.exe d2d_tir_probe.cpp \
//       -ld2d1 -ldwrite -ld3d11 -ldxgi -luuid
// Run (guest, any session; no window, one small offscreen draw per shape):
//   d2d_tir_probe.exe [warp]
//   HELIOS_ICD=nvk chooses NVK for the helios adapter. Next to an
//   application-local DXVK (guest/nvk-rm/windows/stage-dxvk-app.sh) only the
//   D3D11 case runs: Direct2D does not take DXVK's DXGI device there.
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11_3.h>
#include <d2d1_1.h>
#include <d3dcompiler.h>
#include <cmath>
#include <cstdio>
#include <cstring>

typedef HRESULT (STDMETHODCALLTYPE *PfnCreateRs1)(ID3D11Device1*, const D3D11_RASTERIZER_DESC1*,
                                                  ID3D11RasterizerState1**);
static PfnCreateRs1 g_createRs1;

static HRESULT STDMETHODCALLTYPE hookCreateRs1(ID3D11Device1* dev, const D3D11_RASTERIZER_DESC1* desc,
                                               ID3D11RasterizerState1** out) {
  if (desc && desc->ForcedSampleCount)
    printf("rasterizer state: ForcedSampleCount %u\n", desc->ForcedSampleCount);
  return g_createRs1(dev, desc, out);
}

static void hookVtable(void* obj, int index, void* fn, void** orig) {
  void** vtbl = *(void***)obj;
  if (vtbl[index] == fn)
    return;
  DWORD old;
  VirtualProtect(&vtbl[index], sizeof(void*), PAGE_READWRITE, &old);
  *orig = vtbl[index];
  vtbl[index] = fn;
  VirtualProtect(&vtbl[index], sizeof(void*), old, &old);
}


typedef HRESULT (WINAPI *PfnD3DCompile)(LPCVOID, SIZE_T, LPCSTR, const D3D_SHADER_MACRO*, ID3DInclude*,
                                        LPCSTR, LPCSTR, UINT, UINT, ID3DBlob**, ID3DBlob**);

static ID3DBlob* compile(PfnD3DCompile d3dCompile, const char* src, const char* entry, const char* target) {
  ID3DBlob* code = nullptr;
  ID3DBlob* errors = nullptr;
  HRESULT hr = d3dCompile(src, strlen(src), nullptr, nullptr, nullptr, entry, target, 0, 0, &code, &errors);
  if (FAILED(hr)) {
    printf("D3DCompile %s failed: 0x%08lx %s\n", entry, hr,
           errors ? (const char*)errors->GetBufferPointer() : "");
    return nullptr;
  }
  return code;
}

static PfnD3DCompile loadCompiler() {
  HMODULE compiler = LoadLibraryA("d3dcompiler_47.dll");
  PfnD3DCompile d3dCompile = compiler ? (PfnD3DCompile)GetProcAddress(compiler, "D3DCompile") : nullptr;
  if (!d3dCompile)
    printf("d3dcompiler_47.dll not available\n");
  return d3dCompile;
}

static ID3D11RasterizerState1* forcedRasterizerState(ID3D11Device* dev, UINT forced) {
  ID3D11Device1* dev1 = nullptr;
  if (FAILED(dev->QueryInterface(__uuidof(ID3D11Device1), (void**)&dev1))) {
    printf("no ID3D11Device1\n");
    return nullptr;
  }
  D3D11_RASTERIZER_DESC1 rd = {};
  rd.FillMode = D3D11_FILL_SOLID;
  rd.CullMode = D3D11_CULL_NONE;
  rd.DepthClipEnable = TRUE;
  rd.ForcedSampleCount = forced;
  ID3D11RasterizerState1* rs = nullptr;
  HRESULT hr = dev1->CreateRasterizerState1(&rd, &rs);
  dev1->Release();
  if (FAILED(hr)) {
    printf("CreateRasterizerState1 (ForcedSampleCount %u) failed: 0x%08lx\n", forced, hr);
    return nullptr;
  }
  return rs;
}

// The device must survive every case: a GPU fault shows up here.
static bool deviceAlive(ID3D11Device* dev, const char* what) {
  HRESULT hr = dev->GetDeviceRemovedReason();
  if (hr != S_OK)
    printf("%s: device removed, reason 0x%08lx\n", what, hr);
  return hr == S_OK;
}

// One triangle over most of the lower half of the target (its slanted edge
// cuts pixels at varying fractions), rasterized with ForcedSampleCount N into
// the single-sample target; the pixel shader writes the covered fraction of
// the N samples. 16 is Direct2D's case; 8 and 4 are the other raster to
// color sample ratios D3D11 can ask for.
static int drawForcedSampleCount(ID3D11Device* dev, ID3D11DeviceContext* ctx, ID3D11Texture2D* tex,
                                 ID3D11Texture2D* staging, UINT W, UINT H, UINT forced) {
  char hlsl[1024];
  snprintf(hlsl, sizeof(hlsl),
    "float4 vs(uint id : SV_VertexID) : SV_Position {\n"
    "  float2 p[3] = { float2(-1.0, -1.0), float2(1.0, -1.0), float2(-0.37, 1.0) };\n"
    "  return float4(p[id], 0.0, 1.0);\n"
    "}\n"
    "float4 ps(float4 pos : SV_Position, uint cov : SV_Coverage) : SV_Target {\n"
    "  float a = countbits(cov) / %u.0;\n"
    "  return float4(a, a, a, a);\n"
    "}\n", forced);

  PfnD3DCompile d3dCompile = loadCompiler();
  if (!d3dCompile)
    return 1;
  ID3DBlob* vsCode = compile(d3dCompile, hlsl, "vs", "vs_5_0");
  ID3DBlob* psCode = compile(d3dCompile, hlsl, "ps", "ps_5_0");
  if (!vsCode || !psCode)
    return 1;

  ID3D11VertexShader* vs = nullptr;
  ID3D11PixelShader* ps = nullptr;
  dev->CreateVertexShader(vsCode->GetBufferPointer(), vsCode->GetBufferSize(), nullptr, &vs);
  dev->CreatePixelShader(psCode->GetBufferPointer(), psCode->GetBufferSize(), nullptr, &ps);

  ID3D11RasterizerState1* rs = forcedRasterizerState(dev, forced);
  if (!rs)
    return 1;

  ID3D11RenderTargetView* rtv = nullptr;
  dev->CreateRenderTargetView(tex, nullptr, &rtv);
  const float clear[4] = { 0.0f, 0.0f, 0.0f, 0.0f };
  ctx->ClearRenderTargetView(rtv, clear);
  ctx->OMSetRenderTargets(1, &rtv, nullptr);
  D3D11_VIEWPORT vp = { 0.0f, 0.0f, float(W), float(H), 0.0f, 1.0f };
  ctx->RSSetViewports(1, &vp);
  ctx->RSSetState(rs);
  ctx->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
  ctx->VSSetShader(vs, nullptr, 0);
  ctx->PSSetShader(ps, nullptr, 0);
  ctx->Draw(3, 0);
  ctx->OMSetRenderTargets(0, nullptr, nullptr);
  ctx->RSSetState(nullptr);

  ctx->CopyResource(staging, tex);
  D3D11_MAPPED_SUBRESOURCE m;
  HRESULT hr;
  if (FAILED(hr = ctx->Map(staging, 0, D3D11_MAP_READ, 0, &m))) {
    printf("Map failed: 0x%08lx\n", hr);
    return 1;
  }
  unsigned maxAlpha = 0, full = 0, covered = 0;
  bool levels[256] = {};
  for (UINT y = 0; y < H; y++) {
    for (UINT x = 0; x < W; x++) {
      unsigned a = ((const BYTE*)m.pData)[y * m.RowPitch + x * 4 + 3];
      maxAlpha = a > maxAlpha ? a : maxAlpha;
      full += a == 255;
      covered += a != 0;
      levels[a] = true;
    }
  }
  ctx->Unmap(staging, 0);
  unsigned partialLevels = 0;
  for (unsigned a = 1; a < 255; a++)
    partialLevels += levels[a];
  char name[32];
  snprintf(name, sizeof(name), "D3D11 forced %u", forced);
  // Every partial level k/N shows up along a long slanted edge.
  bool ok = maxAlpha == 255 && partialLevels == forced - 1 && deviceAlive(dev, name);
  printf("%-20s max alpha %3u, %5u pixels at 255, %5u covered, %u partial levels  %s\n",
         name, maxAlpha, full, covered, partialLevels, ok ? "PASS" : "FAIL");
  rtv->Release();
  rs->Release();
  vs->Release();
  ps->Release();
  return ok ? 0 : 1;
}

// UAV-only rendering (no render target, no depth-stencil view) with
// ForcedSampleCount 16: the pixel shader stores countbits(SV_Coverage) per
// pixel into an R32_UINT UAV. D3D11 requires 16; a Vulkan device whose
// framebufferNoAttachmentsSampleCounts stops at 8 rasterizes at 8 (DXVK
// clamps to the device limit), which this accepts and says so.
static int drawUavOnly(ID3D11Device* dev, ID3D11DeviceContext* ctx, UINT W, UINT H) {
  static const char* hlsl =
    "RWTexture2D<uint> counts : register(u0);\n"
    "float4 vs(uint id : SV_VertexID) : SV_Position {\n"
    "  float2 p[3] = { float2(-1.0, -1.0), float2(1.0, -1.0), float2(-0.37, 1.0) };\n"
    "  return float4(p[id], 0.0, 1.0);\n"
    "}\n"
    "void ps(float4 pos : SV_Position, uint cov : SV_Coverage) {\n"
    "  counts[uint2(pos.xy)] = countbits(cov);\n"
    "}\n";
  PfnD3DCompile d3dCompile = loadCompiler();
  if (!d3dCompile)
    return 1;
  ID3DBlob* vsCode = compile(d3dCompile, hlsl, "vs", "vs_5_0");
  ID3DBlob* psCode = compile(d3dCompile, hlsl, "ps", "ps_5_0");
  if (!vsCode || !psCode)
    return 1;
  ID3D11VertexShader* vs = nullptr;
  ID3D11PixelShader* ps = nullptr;
  dev->CreateVertexShader(vsCode->GetBufferPointer(), vsCode->GetBufferSize(), nullptr, &vs);
  dev->CreatePixelShader(psCode->GetBufferPointer(), psCode->GetBufferSize(), nullptr, &ps);

  D3D11_TEXTURE2D_DESC td = {};
  td.Width = W;
  td.Height = H;
  td.MipLevels = 1;
  td.ArraySize = 1;
  td.Format = DXGI_FORMAT_R32_UINT;
  td.SampleDesc.Count = 1;
  td.Usage = D3D11_USAGE_DEFAULT;
  td.BindFlags = D3D11_BIND_UNORDERED_ACCESS;
  ID3D11Texture2D* tex = nullptr;
  ID3D11Texture2D* staging = nullptr;
  HRESULT hr;
  if (FAILED(hr = dev->CreateTexture2D(&td, nullptr, &tex))) {
    printf("CreateTexture2D (R32_UINT UAV) failed: 0x%08lx\n", hr);
    return 1;
  }
  td.Usage = D3D11_USAGE_STAGING;
  td.BindFlags = 0;
  td.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
  dev->CreateTexture2D(&td, nullptr, &staging);
  ID3D11UnorderedAccessView* uav = nullptr;
  dev->CreateUnorderedAccessView(tex, nullptr, &uav);
  const UINT zero[4] = { 0, 0, 0, 0 };
  ctx->ClearUnorderedAccessViewUint(uav, zero);

  ID3D11RasterizerState1* rs = forcedRasterizerState(dev, 16);
  if (!rs)
    return 1;
  ctx->OMSetRenderTargetsAndUnorderedAccessViews(0, nullptr, nullptr, 0, 1, &uav, nullptr);
  D3D11_VIEWPORT vp = { 0.0f, 0.0f, float(W), float(H), 0.0f, 1.0f };
  ctx->RSSetViewports(1, &vp);
  ctx->RSSetState(rs);
  ctx->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
  ctx->VSSetShader(vs, nullptr, 0);
  ctx->PSSetShader(ps, nullptr, 0);
  ctx->Draw(3, 0);
  ctx->OMSetRenderTargets(0, nullptr, nullptr);
  ctx->RSSetState(nullptr);

  ctx->CopyResource(staging, tex);
  D3D11_MAPPED_SUBRESOURCE m;
  if (FAILED(hr = ctx->Map(staging, 0, D3D11_MAP_READ, 0, &m))) {
    printf("Map failed: 0x%08lx\n", hr);
    return 1;
  }
  unsigned maxCount = 0, covered = 0, bad = 0;
  bool levels[33] = {};
  for (UINT y = 0; y < H; y++) {
    for (UINT x = 0; x < W; x++) {
      unsigned c = ((const UINT*)((const BYTE*)m.pData + y * m.RowPitch))[x];
      if (c > 32) {
        bad++;
        continue;
      }
      maxCount = c > maxCount ? c : maxCount;
      covered += c != 0;
      levels[c] = true;
    }
  }
  // A pixel deep inside the triangle has every sample covered.
  unsigned center = ((const UINT*)((const BYTE*)m.pData + (H - 8) * m.RowPitch))[W / 2];
  ctx->Unmap(staging, 0);
  unsigned partialLevels = 0;
  for (unsigned c = 1; c < maxCount; c++)
    partialLevels += levels[c];
  bool ok = !bad && (maxCount == 16 || maxCount == 8) && center == maxCount
         && partialLevels == maxCount - 1 && deviceAlive(dev, "UAV-only forced 16");
  printf("%-20s max count %2u (interior %2u), %5u covered, %u partial levels%s  %s\n",
         "UAV-only forced 16", maxCount, center, covered, partialLevels,
         maxCount == 8 ? " (device limit 8)" : "", ok ? "PASS" : "FAIL");
  rs->Release();
  uav->Release();
  tex->Release();
  staging->Release();
  vs->Release();
  ps->Release();
  return ok ? 0 : 1;
}

// ForcedSampleCount 16 with a depth-stencil view bound (depth test off):
// D3D11 leaves this undefined, so only the device surviving is checked; the
// driver must not combine target-independent rasterization with a depth
// attachment.
static int drawWithDepth(ID3D11Device* dev, ID3D11DeviceContext* ctx, ID3D11Texture2D* tex,
                         UINT W, UINT H) {
  static const char* hlsl =
    "float4 vs(uint id : SV_VertexID) : SV_Position {\n"
    "  float2 p[3] = { float2(-1.0, -1.0), float2(1.0, -1.0), float2(-0.37, 1.0) };\n"
    "  return float4(p[id], 0.5, 1.0);\n"
    "}\n"
    "float4 ps(float4 pos : SV_Position, uint cov : SV_Coverage) : SV_Target {\n"
    "  float a = countbits(cov) / 16.0;\n"
    "  return float4(a, a, a, a);\n"
    "}\n";
  PfnD3DCompile d3dCompile = loadCompiler();
  if (!d3dCompile)
    return 1;
  ID3DBlob* vsCode = compile(d3dCompile, hlsl, "vs", "vs_5_0");
  ID3DBlob* psCode = compile(d3dCompile, hlsl, "ps", "ps_5_0");
  if (!vsCode || !psCode)
    return 1;
  ID3D11VertexShader* vs = nullptr;
  ID3D11PixelShader* ps = nullptr;
  dev->CreateVertexShader(vsCode->GetBufferPointer(), vsCode->GetBufferSize(), nullptr, &vs);
  dev->CreatePixelShader(psCode->GetBufferPointer(), psCode->GetBufferSize(), nullptr, &ps);

  D3D11_TEXTURE2D_DESC td = {};
  td.Width = W;
  td.Height = H;
  td.MipLevels = 1;
  td.ArraySize = 1;
  td.Format = DXGI_FORMAT_D24_UNORM_S8_UINT;
  td.SampleDesc.Count = 1;
  td.Usage = D3D11_USAGE_DEFAULT;
  td.BindFlags = D3D11_BIND_DEPTH_STENCIL;
  ID3D11Texture2D* depth = nullptr;
  HRESULT hr;
  if (FAILED(hr = dev->CreateTexture2D(&td, nullptr, &depth))) {
    printf("CreateTexture2D (D24S8) failed: 0x%08lx\n", hr);
    return 1;
  }
  ID3D11DepthStencilView* dsv = nullptr;
  dev->CreateDepthStencilView(depth, nullptr, &dsv);
  D3D11_DEPTH_STENCIL_DESC dd = {};
  dd.DepthEnable = FALSE;
  dd.StencilEnable = FALSE;
  ID3D11DepthStencilState* ds = nullptr;
  dev->CreateDepthStencilState(&dd, &ds);
  ID3D11RenderTargetView* rtv = nullptr;
  dev->CreateRenderTargetView(tex, nullptr, &rtv);
  ID3D11RasterizerState1* rs = forcedRasterizerState(dev, 16);
  if (!rs)
    return 1;

  const float clear[4] = { 0.0f, 0.0f, 0.0f, 0.0f };
  ctx->ClearRenderTargetView(rtv, clear);
  ctx->ClearDepthStencilView(dsv, D3D11_CLEAR_DEPTH | D3D11_CLEAR_STENCIL, 1.0f, 0);
  ctx->OMSetRenderTargets(1, &rtv, dsv);
  ctx->OMSetDepthStencilState(ds, 0);
  D3D11_VIEWPORT vp = { 0.0f, 0.0f, float(W), float(H), 0.0f, 1.0f };
  ctx->RSSetViewports(1, &vp);
  ctx->RSSetState(rs);
  ctx->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
  ctx->VSSetShader(vs, nullptr, 0);
  ctx->PSSetShader(ps, nullptr, 0);
  ctx->Draw(3, 0);
  // Clear the depth-stencil view again with the state still set: a clear
  // with target-independent rasterization on is a class error.
  ctx->ClearDepthStencilView(dsv, D3D11_CLEAR_DEPTH, 0.5f, 0);
  ctx->Draw(3, 0);
  ctx->OMSetRenderTargets(0, nullptr, nullptr);
  ctx->OMSetDepthStencilState(nullptr, 0);
  ctx->RSSetState(nullptr);
  ctx->Flush();

  // Wait for the GPU with a query, then check the device.
  D3D11_QUERY_DESC qd = { D3D11_QUERY_EVENT, 0 };
  ID3D11Query* q = nullptr;
  dev->CreateQuery(&qd, &q);
  ctx->End(q);
  BOOL done = FALSE;
  for (int i = 0; i < 5000 && ctx->GetData(q, &done, sizeof(done), 0) != S_OK; i++)
    Sleep(1);
  bool ok = deviceAlive(dev, "forced 16 + depth");
  printf("%-20s device %s  %s\n", "forced 16 + depth", ok ? "alive" : "removed", ok ? "PASS" : "FAIL");
  q->Release();
  rs->Release();
  rtv->Release();
  ds->Release();
  dsv->Release();
  depth->Release();
  vs->Release();
  ps->Release();
  return ok ? 0 : 1;
}

// Pull-model interpolation at sample indices 8 to 15 of the forced 16-sample
// pattern (D3D11: "Pull Model attribute interpolation is based on the
// ForcedSampleCount pattern when selecting sample location by sample
// index"). The attribute is the pixel position, so its value at sample i
// minus its value at the pixel center is sample i's offset; the pixel
// shader compares all of 8..15 against the standard 16-sample pattern and
// writes 1 per matching index into its own bit of the output.
static int drawEvaluateAtSample(ID3D11Device* dev, ID3D11DeviceContext* ctx, ID3D11Texture2D* tex,
                                ID3D11Texture2D* staging, UINT W, UINT H) {
  static const char* hlsl =
    "struct V { float4 pos : SV_Position; float2 px : TEXCOORD0; };\n"
    "V vs(uint id : SV_VertexID) {\n"
    "  float2 p[3] = { float2(-1.0, -1.0), float2(3.0, -1.0), float2(-1.0, 3.0) };\n"
    "  V v;\n"
    "  v.pos = float4(p[id], 0.0, 1.0);\n"
    "  v.px = (p[id] * float2(0.5, -0.5) + 0.5) * float2(W, H);\n"
    "  return v;\n"
    "}\n"
    "static const int2 pattern[16] = {\n"
    "  int2(1, 1), int2(-1, -3), int2(-3, 2), int2(4, -1), int2(-5, -2), int2(2, 5), int2(5, 3), int2(3, -5),\n"
    "  int2(-2, 6), int2(0, -7), int2(-4, -6), int2(-6, 4), int2(-8, 0), int2(7, -4), int2(6, 7), int2(-7, -8) };\n"
    "float4 ps(V v) : SV_Target {\n"
    "  float2 center = floor(v.pos.xy) + 0.5;\n"
    "  uint ok = 0;\n"
    "  [unroll] for (uint i = 8; i < 16; i++) {\n"
    "    float2 off = (EvaluateAttributeAtSample(v.px, i) - center) * 16.0;\n"
    "    if (all(abs(off - float2(pattern[i])) < 0.25))\n"
    "      ok |= 1u << (i - 8);\n"
    "  }\n"
    "  return float4(ok / 255.0, 0.0, 0.0, 1.0);\n"
    "}\n";
  char src[2048];
  snprintf(src, sizeof(src), "#define W %u.0\n#define H %u.0\n%s", W, H, hlsl);
  PfnD3DCompile d3dCompile = loadCompiler();
  if (!d3dCompile)
    return 1;
  ID3DBlob* vsCode = compile(d3dCompile, src, "vs", "vs_5_0");
  ID3DBlob* psCode = compile(d3dCompile, src, "ps", "ps_5_0");
  if (!vsCode || !psCode)
    return 1;
  ID3D11VertexShader* vs = nullptr;
  ID3D11PixelShader* ps = nullptr;
  dev->CreateVertexShader(vsCode->GetBufferPointer(), vsCode->GetBufferSize(), nullptr, &vs);
  dev->CreatePixelShader(psCode->GetBufferPointer(), psCode->GetBufferSize(), nullptr, &ps);
  ID3D11RasterizerState1* rs = forcedRasterizerState(dev, 16);
  if (!rs)
    return 1;
  ID3D11RenderTargetView* rtv = nullptr;
  dev->CreateRenderTargetView(tex, nullptr, &rtv);
  const float clear[4] = { 0.0f, 0.0f, 0.0f, 0.0f };
  ctx->ClearRenderTargetView(rtv, clear);
  ctx->OMSetRenderTargets(1, &rtv, nullptr);
  D3D11_VIEWPORT vp = { 0.0f, 0.0f, float(W), float(H), 0.0f, 1.0f };
  ctx->RSSetViewports(1, &vp);
  ctx->RSSetState(rs);
  ctx->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
  ctx->VSSetShader(vs, nullptr, 0);
  ctx->PSSetShader(ps, nullptr, 0);
  ctx->Draw(3, 0);
  ctx->OMSetRenderTargets(0, nullptr, nullptr);
  ctx->RSSetState(nullptr);

  ctx->CopyResource(staging, tex);
  D3D11_MAPPED_SUBRESOURCE m;
  HRESULT hr;
  if (FAILED(hr = ctx->Map(staging, 0, D3D11_MAP_READ, 0, &m))) {
    printf("Map failed: 0x%08lx\n", hr);
    return 1;
  }
  // BGRA: red is byte 2. Every pixel is covered by the big triangle.
  unsigned allOk = 0, andBits = 0xff;
  for (UINT y = 0; y < H; y++) {
    for (UINT x = 0; x < W; x++) {
      unsigned r = ((const BYTE*)m.pData)[y * m.RowPitch + x * 4 + 2];
      allOk += r == 0xff;
      andBits &= r;
    }
  }
  ctx->Unmap(staging, 0);
  bool ok = allOk == W * H && deviceAlive(dev, "EvaluateAtSample 8-15");
  printf("%-20s %5u of %u pixels match the 16-sample pattern at 8..15 (common bits 0x%02x)  %s\n",
         "EvaluateAtSample 8-15", allOk, W * H, andBits, ok ? "PASS" : "FAIL");
  rtv->Release();
  rs->Release();
  vs->Release();
  ps->Release();
  return ok ? 0 : 1;
}

int main(int argc, char** argv) {
  const bool warp = argc > 1 && !strcmp(argv[1], "warp");
  const UINT W = 128, H = 128;

  D3D_FEATURE_LEVEL fl = D3D_FEATURE_LEVEL_11_1, got;
  ID3D11Device* dev = nullptr;
  ID3D11DeviceContext* ctx = nullptr;
  HRESULT hr = D3D11CreateDevice(nullptr, warp ? D3D_DRIVER_TYPE_WARP : D3D_DRIVER_TYPE_HARDWARE, nullptr,
                                 D3D11_CREATE_DEVICE_BGRA_SUPPORT, &fl, 1, D3D11_SDK_VERSION, &dev, &got, &ctx);
  if (FAILED(hr)) {
    printf("D3D11CreateDevice (FL 11_1) failed: 0x%08lx\n", hr);
    return 1;
  }
  printf("device: %s, FL 0x%x\n", warp ? "WARP" : "hardware", got);

  // ID3D11Device is IUnknown (3) + 40 methods; ID3D11Device1 continues with
  // GetImmediateContext1, CreateDeferredContext1, CreateBlendState1 and
  // CreateRasterizerState1 at 46.
  ID3D11Device1* dev1 = nullptr;
  if (SUCCEEDED(dev->QueryInterface(__uuidof(ID3D11Device1), (void**)&dev1)))
    hookVtable(dev1, 46, (void*)hookCreateRs1, (void**)&g_createRs1);

  D3D11_TEXTURE2D_DESC td = {};
  td.Width = W;
  td.Height = H;
  td.MipLevels = 1;
  td.ArraySize = 1;
  td.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
  td.SampleDesc.Count = 1;
  td.Usage = D3D11_USAGE_DEFAULT;
  td.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
  ID3D11Texture2D* tex = nullptr;
  if (FAILED(hr = dev->CreateTexture2D(&td, nullptr, &tex))) {
    printf("CreateTexture2D failed: 0x%08lx\n", hr);
    return 1;
  }
  td.Usage = D3D11_USAGE_STAGING;
  td.BindFlags = 0;
  td.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
  ID3D11Texture2D* staging = nullptr;
  if (FAILED(hr = dev->CreateTexture2D(&td, nullptr, &staging))) {
    printf("CreateTexture2D (staging) failed: 0x%08lx\n", hr);
    return 1;
  }

  // Twice: the second clear and draw follow a draw that left
  // target-independent rasterization on in the driver.
  int failures = drawForcedSampleCount(dev, ctx, tex, staging, W, H, 16);
  failures += drawForcedSampleCount(dev, ctx, tex, staging, W, H, 16);
  // The other raster to color sample ratios
  failures += drawForcedSampleCount(dev, ctx, tex, staging, W, H, 8);
  failures += drawForcedSampleCount(dev, ctx, tex, staging, W, H, 4);
  failures += drawUavOnly(dev, ctx, W, H);
  failures += drawWithDepth(dev, ctx, tex, W, H);
  failures += drawEvaluateAtSample(dev, ctx, tex, staging, W, H);
  // And the Direct2D case once more after all of that
  failures += drawForcedSampleCount(dev, ctx, tex, staging, W, H, 16);

  ID2D1Factory1* factory = nullptr;
  D2D1_FACTORY_OPTIONS fo = {};
  if (FAILED(hr = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, __uuidof(ID2D1Factory1), &fo,
                                    (void**)&factory))) {
    printf("D2D1CreateFactory failed: 0x%08lx\n", hr);
    return 1;
  }
  IDXGIDevice* dxgi = nullptr;
  dev->QueryInterface(__uuidof(IDXGIDevice), (void**)&dxgi);
  ID2D1Device* d2dDev = nullptr;
  if (FAILED(hr = factory->CreateDevice(dxgi, &d2dDev))) {
    printf("ID2D1Factory1::CreateDevice failed: 0x%08lx, Direct2D cases skipped\n", hr);
    printf("RESULT: %s\n", failures ? "FAIL" : "PASS");
    return failures ? 2 : 0;
  }
  ID2D1DeviceContext* dc = nullptr;
  d2dDev->CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE, &dc);

  IDXGISurface* surface = nullptr;
  tex->QueryInterface(__uuidof(IDXGISurface), (void**)&surface);
  D2D1_BITMAP_PROPERTIES1 bp = {};
  bp.pixelFormat.format = DXGI_FORMAT_B8G8R8A8_UNORM;
  bp.pixelFormat.alphaMode = D2D1_ALPHA_MODE_PREMULTIPLIED;
  bp.dpiX = bp.dpiY = 96.0f;
  bp.bitmapOptions = D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW;
  ID2D1Bitmap1* target = nullptr;
  if (FAILED(hr = dc->CreateBitmapFromDxgiSurface(surface, &bp, &target))) {
    printf("CreateBitmapFromDxgiSurface failed: 0x%08lx\n", hr);
    return 1;
  }
  dc->SetTarget(target);
  dc->SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);

  ID2D1SolidColorBrush* brush = nullptr;
  dc->CreateSolidColorBrush(D2D1::ColorF(1.0f, 1.0f, 1.0f, 1.0f), &brush);

  ID2D1EllipseGeometry* ellipse = nullptr;
  factory->CreateEllipseGeometry(D2D1::Ellipse(D2D1::Point2F(64.0f, 64.0f), 60.0f, 60.0f), &ellipse);

  ID2D1GeometrySink* sink = nullptr;
  ID2D1PathGeometry* diamond = nullptr;
  factory->CreatePathGeometry(&diamond);
  diamond->Open(&sink);
  sink->BeginFigure(D2D1::Point2F(64.0f, 8.3f), D2D1_FIGURE_BEGIN_FILLED);
  sink->AddLine(D2D1::Point2F(119.7f, 64.0f));
  sink->AddLine(D2D1::Point2F(64.0f, 119.7f));
  sink->AddLine(D2D1::Point2F(8.3f, 64.0f));
  sink->EndFigure(D2D1_FIGURE_END_CLOSED);
  sink->Close();
  sink->Release();

  // Complex enough for Direct2D to fill it with TIR instead of a mesh.
  ID2D1PathGeometry* star = nullptr;
  factory->CreatePathGeometry(&star);
  star->Open(&sink);
  sink->SetFillMode(D2D1_FILL_MODE_WINDING);
  for (int i = 0; i < 401; i++) {
    float r = (i & 1) ? 30.0f : 60.0f, a = i * 3.14159265f / 200.0f;
    D2D1_POINT_2F pt = D2D1::Point2F(64.0f + r * cosf(a), 64.0f + r * sinf(a));
    if (i == 0)
      sink->BeginFigure(pt, D2D1_FIGURE_BEGIN_FILLED);
    else
      sink->AddLine(pt);
  }
  sink->EndFigure(D2D1_FIGURE_END_CLOSED);
  sink->BeginFigure(D2D1::Point2F(64.0f, 10.0f), D2D1_FIGURE_BEGIN_FILLED);
  for (int i = 1; i <= 64; i++) {
    float a0 = (i - 1) * 3.14159265f / 32.0f, a1 = i * 3.14159265f / 32.0f;
    sink->AddBezier(D2D1::BezierSegment(
      D2D1::Point2F(64.0f + 70.0f * sinf(a0 + 0.02f), 64.0f - 70.0f * cosf(a0 + 0.02f)),
      D2D1::Point2F(64.0f + 20.0f * sinf(a1 - 0.02f), 64.0f - 20.0f * cosf(a1 - 0.02f)),
      D2D1::Point2F(64.0f + 54.0f * sinf(a1), 64.0f - 54.0f * cosf(a1))));
  }
  sink->EndFigure(D2D1_FIGURE_END_CLOSED);
  sink->Close();
  sink->Release();

  struct Case {
    const char* name;
    ID2D1Geometry* geometry;
    float rotation;
  } cases[] = {
    { "diamond (mesh)", diamond, 0.0f },
    { "ellipse (mesh)", ellipse, 0.0f },
    { "star (TIR)", star, 0.0f },
    { "star rotated (TIR)", star, 7.0f },
  };

  for (const Case& c : cases) {
    dc->BeginDraw();
    dc->Clear(D2D1::ColorF(0.0f, 0.0f, 0.0f, 0.0f));
    dc->SetTransform(D2D1::Matrix3x2F::Rotation(c.rotation, D2D1::Point2F(64.0f, 64.0f)));
    dc->FillGeometry(c.geometry, brush);
    dc->SetTransform(D2D1::Matrix3x2F::Identity());
    if (FAILED(hr = dc->EndDraw())) {
      printf("EndDraw failed: 0x%08lx\n", hr);
      return 1;
    }

    ctx->CopyResource(staging, tex);
    D3D11_MAPPED_SUBRESOURCE m;
    if (FAILED(hr = ctx->Map(staging, 0, D3D11_MAP_READ, 0, &m))) {
      printf("Map failed: 0x%08lx\n", hr);
      return 1;
    }
    unsigned maxAlpha = 0, full = 0, covered = 0;
    for (UINT y = 0; y < H; y++) {
      for (UINT x = 0; x < W; x++) {
        unsigned a = ((const BYTE*)m.pData)[y * m.RowPitch + x * 4 + 3];
        maxAlpha = a > maxAlpha ? a : maxAlpha;
        full += a == 255;
        covered += a != 0;
      }
    }
    ctx->Unmap(staging, 0);
    printf("%-20s max alpha %3u, %5u pixels at 255, %5u covered\n", c.name, maxAlpha, full, covered);
    failures += maxAlpha != 255;
  }
  printf("RESULT: %s\n", failures ? "FAIL" : "PASS");
  return failures ? 2 : 0;
}
