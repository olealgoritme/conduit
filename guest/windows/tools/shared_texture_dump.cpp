// shared_texture_dump.cpp: open another process's KMT-shared D3D11 texture by
// its global handle (the UMD log's "global=0x..." of an OpenDdiTexture2D or
// "KMT handles stamped" line), copy it to a staging texture and write it as a
// BMP, with a count of non-zero texels. Shows what a consumer of the shared
// memory actually reads, e.g. whether a glyph atlas holds glyphs.
//
// Build (host):
//   x86_64-w64-mingw32-g++ -O2 -static -o shared_texture_dump.exe shared_texture_dump.cpp \
//       -ld3d11 -ldxgi -ldxguid -luuid
// Run (guest): shared_texture_dump.exe <hex handle> <out.bmp>   (HELIOS_ICD as usual)
//              shared_texture_dump.exe scan <W>x<H> <out-prefix>
//   scan: try every KMT share handle 0xC0000002..0xC003FFFE (step 4) and dump
//   each openable texture of that size (another process's surface whose
//   handle is not known: DirectComposition and XAML do not log theirs).
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11.h>
#include <dxgi1_2.h>
#include <cstdio>
#include <cstdint>
#include <cstdlib>
#include <cwchar>
#include <vector>

static int Dump(ID3D11Device* dev, ID3D11DeviceContext* ctx, ID3D11Texture2D* tex, const char* out);

int main(int argc, char** argv) {
  if (argc < 3) { printf("usage: %s <hex handle> <out.bmp> | scan WxH prefix\n", argv[0]); return 2; }
  IDXGIFactory1* f = nullptr; CreateDXGIFactory1(__uuidof(IDXGIFactory1), (void**)&f);
  IDXGIAdapter1* a = nullptr; IDXGIAdapter1* chosen = nullptr;
  for (UINT i = 0; f->EnumAdapters1(i, &a) != DXGI_ERROR_NOT_FOUND; ++i) {
    DXGI_ADAPTER_DESC1 d{}; a->GetDesc1(&d);
    if (!chosen && wcsstr(d.Description, L"Helios")) { chosen = a; continue; }
    a->Release();
  }
  ID3D11Device* dev = nullptr; ID3D11DeviceContext* ctx = nullptr;
  HRESULT hr = D3D11CreateDevice(chosen, D3D_DRIVER_TYPE_UNKNOWN, nullptr, 0, nullptr, 0,
                                 D3D11_SDK_VERSION, &dev, nullptr, &ctx);
  if (FAILED(hr)) { printf("D3D11CreateDevice hr=0x%08lx\n", hr); return 3; }
  if (strcmp(argv[1], "scan") == 0) {
    UINT w = 0, hh = 0;
    sscanf(argv[2], "%ux%u", &w, &hh);
    const char* prefix = argc > 3 ? argv[3] : "scan";
    int found = 0;
    for (uintptr_t v = 0xC0000002; v < 0xC0040000; v += 4) {
      ID3D11Texture2D* t = nullptr;
      if (FAILED(dev->OpenSharedResource((HANDLE)v, __uuidof(ID3D11Texture2D), (void**)&t)) || !t) continue;
      D3D11_TEXTURE2D_DESC d; t->GetDesc(&d);
      printf("handle 0x%llx: %ux%u fmt=%d\n", (unsigned long long)v, d.Width, d.Height, d.Format);
      if ((!w || d.Width == w) && (!hh || d.Height == hh)) {
        char path[512]; snprintf(path, sizeof(path), "%s_%llx.bmp", prefix, (unsigned long long)v);
        Dump(dev, ctx, t, path);
        found++;
      }
      t->Release();
    }
    printf("%d matching texture(s)\n", found);
    return 0;
  }
  HANDLE h = (HANDLE)(uintptr_t)strtoull(argv[1], nullptr, 16);
  ID3D11Texture2D* tex = nullptr;
  hr = dev->OpenSharedResource(h, __uuidof(ID3D11Texture2D), (void**)&tex);
  printf("OpenSharedResource(0x%p) hr=0x%08lx\n", h, hr);
  if (FAILED(hr)) return 4;
  return Dump(dev, ctx, tex, argv[2]);
}

static int Dump(ID3D11Device* dev, ID3D11DeviceContext* ctx, ID3D11Texture2D* tex, const char* out) {
  D3D11_TEXTURE2D_DESC d; tex->GetDesc(&d);
  printf("%ux%u fmt=%d bind=0x%x misc=0x%x\n", d.Width, d.Height, d.Format, d.BindFlags, d.MiscFlags);
  D3D11_TEXTURE2D_DESC sd = d;
  sd.Usage = D3D11_USAGE_STAGING; sd.BindFlags = 0; sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ; sd.MiscFlags = 0;
  ID3D11Texture2D* st = nullptr;
  if (FAILED(dev->CreateTexture2D(&sd, nullptr, &st))) return 5;
  ctx->CopyResource(st, tex);
  D3D11_MAPPED_SUBRESOURCE m{};
  if (FAILED(ctx->Map(st, 0, D3D11_MAP_READ, 0, &m))) return 6;
  const bool one = d.Format == DXGI_FORMAT_A8_UNORM || d.Format == DXGI_FORMAT_R8_UNORM;
  std::vector<uint8_t> img((size_t)d.Width * d.Height * 4);
  size_t nonzero = 0;
  for (UINT y = 0; y < d.Height; ++y)
    for (UINT x = 0; x < d.Width; ++x) {
      const uint8_t* s = (const uint8_t*)m.pData + (size_t)y * m.RowPitch + (one ? x : x * 4);
      uint8_t* o = &img[((size_t)y * d.Width + x) * 4];
      if (one) { o[0] = o[1] = o[2] = 255 - s[0]; o[3] = 255; nonzero += s[0] != 0; }
      else { o[0] = s[0]; o[1] = s[1]; o[2] = s[2]; o[3] = 255; nonzero += (s[0] | s[1] | s[2] | s[3]) != 0; }
    }
  ctx->Unmap(st, 0);
  printf("non-zero texels: %zu of %u\n", nonzero, d.Width * d.Height);
  FILE* fp = fopen(out, "wb");
  if (fp) {
    uint32_t sz = d.Width * d.Height * 4;
    uint8_t hdr[54] = {'B', 'M'};
    *(uint32_t*)(hdr + 2) = 54 + sz; *(uint32_t*)(hdr + 10) = 54; *(uint32_t*)(hdr + 14) = 40;
    *(int32_t*)(hdr + 18) = d.Width; *(int32_t*)(hdr + 22) = -(int32_t)d.Height;
    *(uint16_t*)(hdr + 26) = 1; *(uint16_t*)(hdr + 28) = 32; *(uint32_t*)(hdr + 34) = sz;
    fwrite(hdr, 1, 54, fp); fwrite(img.data(), 1, sz, fp); fclose(fp);
  }
  st->Release();
  return 0;
}
