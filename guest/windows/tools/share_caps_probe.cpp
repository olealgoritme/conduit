// share_caps_probe: which formats the D3D11 runtime lets a driver share, per
// share kind, compared with WARP (Microsoft's software driver behind the same
// runtime, the reference for runtime policy).
//
//   share_caps_probe.exe [hw|warp|both=both]
//
// Per device: D3D11_OPTIONS.ExtendedResourceSharing, the DDI interface the
// runtime negotiated (via the feature level), and per format:
// FORMAT_SUPPORT (texture2d/render target/shader sample), FORMAT_SUPPORT2
// SHAREABLE, and CreateTexture2D 64x64 with MISC_SHARED (KMT),
// MISC_SHARED_KEYEDMUTEX, MISC_SHARED_NTHANDLE|SHARED,
// MISC_SHARED_NTHANDLE|KEYEDMUTEX, plus CreateSharedHandle for the NT ones.
// Each result is an HRESULT (0 = ok).
//
// HELIOS_ICD=nvk / venus selects the Helios backend for the hw device.
//
// Build: x86_64-w64-mingw32-g++ -std=c++17 -O2 -static share_caps_probe.cpp
//          -o share_caps_probe.exe -ld3d11 -ldxgi -luuid
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11_1.h>
#include <dxgi1_2.h>

#include <cstdio>
#include <cstring>

struct Fmt {
  const char *name;
  DXGI_FORMAT f;
};
static const Fmt kFmts[] = {
    {"bgra8", DXGI_FORMAT_B8G8R8A8_UNORM},     {"rgba8", DXGI_FORMAT_R8G8B8A8_UNORM},
    {"rgb10a2", DXGI_FORMAT_R10G10B10A2_UNORM}, {"rgba16f", DXGI_FORMAT_R16G16B16A16_FLOAT},
    {"a8", DXGI_FORMAT_A8_UNORM},              {"r8", DXGI_FORMAT_R8_UNORM},
    {"r8g8", DXGI_FORMAT_R8G8_UNORM},          {"r16", DXGI_FORMAT_R16_UNORM},
    {"r16f", DXGI_FORMAT_R16_FLOAT},           {"r16g16", DXGI_FORMAT_R16G16_UNORM},
    {"r16g16f", DXGI_FORMAT_R16G16_FLOAT},     {"b5g6r5", DXGI_FORMAT_B5G6R5_UNORM},
    {"b5g5r5a1", DXGI_FORMAT_B5G5R5A1_UNORM},  {"b4g4r4a4", DXGI_FORMAT_B4G4R4A4_UNORM},
    {"r32f", DXGI_FORMAT_R32_FLOAT},           {"rgba16", DXGI_FORMAT_R16G16B16A16_UNORM},
    {"nv12", DXGI_FORMAT_NV12},                {"p010", DXGI_FORMAT_P010},
    {"p016", DXGI_FORMAT_P016},                {"yuy2", DXGI_FORMAT_YUY2},
};

static void probe(const char *label, D3D_DRIVER_TYPE type) {
  ID3D11Device *dev = nullptr;
  ID3D11DeviceContext *ctx = nullptr;
  D3D_FEATURE_LEVEL want[] = {D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0}, got = {};
  HRESULT hr = D3D11CreateDevice(nullptr, type, nullptr, D3D11_CREATE_DEVICE_BGRA_SUPPORT, want, 2, D3D11_SDK_VERSION,
                                 &dev, &got, &ctx);
  if (FAILED(hr)) {
    std::printf("%s: D3D11CreateDevice 0x%08lx\n", label, (unsigned long)hr);
    return;
  }
  D3D11_FEATURE_DATA_D3D11_OPTIONS o = {};
  dev->CheckFeatureSupport(D3D11_FEATURE_D3D11_OPTIONS, &o, sizeof(o));
  IDXGIDevice *dx = nullptr;
  IDXGIAdapter *ad = nullptr;
  DXGI_ADAPTER_DESC ades = {};
  if (SUCCEEDED(dev->QueryInterface(__uuidof(IDXGIDevice), (void **)&dx)) && SUCCEEDED(dx->GetAdapter(&ad)))
    ad->GetDesc(&ades);
  LARGE_INTEGER umd = {};
  if (ad) ad->CheckInterfaceSupport(__uuidof(IDXGIDevice), &umd);
  std::printf("%s: adapter \"%ls\" FL 0x%x, UMD version %u.%u.%u.%u, ExtendedResourceSharing %d\n", label,
              ades.Description, got, HIWORD(umd.HighPart), LOWORD(umd.HighPart), HIWORD(umd.LowPart),
              LOWORD(umd.LowPart), o.ExtendedResourceSharing);
  std::printf("  %-9s %-6s %-5s %-10s %-10s %-10s %-10s %-10s %-10s\n", "format", "tex2d", "shr2", "kmt", "kmt-keyed",
              "nt", "nt-keyed", "nt-handle", "ntk-handle");
  for (const Fmt &f : kFmts) {
    D3D11_FEATURE_DATA_FORMAT_SUPPORT fs = {f.f, 0};
    D3D11_FEATURE_DATA_FORMAT_SUPPORT2 fs2 = {f.f, 0};
    dev->CheckFeatureSupport(D3D11_FEATURE_FORMAT_SUPPORT, &fs, sizeof(fs));
    dev->CheckFeatureSupport(D3D11_FEATURE_FORMAT_SUPPORT2, &fs2, sizeof(fs2));
    const UINT misc[4] = {D3D11_RESOURCE_MISC_SHARED, D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX,
                          D3D11_RESOURCE_MISC_SHARED_NTHANDLE | D3D11_RESOURCE_MISC_SHARED,
                          D3D11_RESOURCE_MISC_SHARED_NTHANDLE | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX};
    HRESULT res[4], sh[2] = {E_FAIL, E_FAIL};
    for (int k = 0; k < 4; k++) {
      D3D11_TEXTURE2D_DESC td = {};
      td.Width = td.Height = 64;
      td.MipLevels = td.ArraySize = 1;
      td.Format = f.f;
      td.SampleDesc.Count = 1;
      td.Usage = D3D11_USAGE_DEFAULT;
      const bool yuv = f.f == DXGI_FORMAT_NV12 || f.f == DXGI_FORMAT_P010 || f.f == DXGI_FORMAT_P016 ||
                       f.f == DXGI_FORMAT_YUY2;
      td.BindFlags = D3D11_BIND_SHADER_RESOURCE |
                     ((fs.OutFormatSupport & D3D11_FORMAT_SUPPORT_RENDER_TARGET) && !yuv ? D3D11_BIND_RENDER_TARGET : 0);
      if (yuv) td.BindFlags = D3D11_BIND_SHADER_RESOURCE;
      td.MiscFlags = misc[k];
      ID3D11Texture2D *t = nullptr;
      res[k] = dev->CreateTexture2D(&td, nullptr, &t);
      if (SUCCEEDED(res[k]) && k >= 2) {
        IDXGIResource1 *r = nullptr;
        HANDLE h = nullptr;
        sh[k - 2] = t->QueryInterface(__uuidof(IDXGIResource1), (void **)&r);
        if (SUCCEEDED(sh[k - 2])) sh[k - 2] = r->CreateSharedHandle(nullptr, DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE, nullptr, &h);
        if (h) CloseHandle(h);
        if (r) r->Release();
      }
      if (t) t->Release();
    }
    std::printf("  %-9s %-6s %-5s %08lx   %08lx   %08lx   %08lx   %08lx   %08lx\n", f.name,
                (fs.OutFormatSupport & D3D11_FORMAT_SUPPORT_TEXTURE2D) ? "yes" : "no",
                (fs2.OutFormatSupport2 & D3D11_FORMAT_SUPPORT2_SHAREABLE) ? "yes" : "no", (unsigned long)res[0],
                (unsigned long)res[1], (unsigned long)res[2], (unsigned long)res[3], (unsigned long)sh[0],
                (unsigned long)sh[1]);
  }
  if (ad) ad->Release();
  if (dx) dx->Release();
  ctx->Release();
  dev->Release();
}

int main(int argc, char **argv) {
  setvbuf(stdout, nullptr, _IONBF, 0);
  const char *which = argc > 1 ? argv[1] : "both";
  std::printf("share_caps_probe, HELIOS_ICD=%s\n", std::getenv("HELIOS_ICD") ? std::getenv("HELIOS_ICD") : "(unset)");
  if (std::strcmp(which, "warp")) probe("hw", D3D_DRIVER_TYPE_HARDWARE);
  if (std::strcmp(which, "hw")) probe("warp", D3D_DRIVER_TYPE_WARP);
  return 0;
}
