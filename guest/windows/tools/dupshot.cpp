// dupshot.cpp: a screenshot through DXGI Desktop Duplication, independent of the GDI screen read
// (CopyFromScreen) and of host-side capture.
//
// IDXGIOutput1::DuplicateOutput -> AcquireNextFrame -> CopyResource into a CPU-readable staging
// texture -> PNG (WIC). The mouse pointer is not drawn (duplication delivers it separately; this
// tool ignores it). Prints one line per output:
//   DUPSHOT ok path=<png> size=<w>x<h> format=<dxgi> nonblack=<fraction> frames=<accumulated>
//   DUPSHOT fail step=<what> hr=0x<HRESULT>
// "nonblack" is the fraction of pixels with any of R, G, B above 8.
//
// Build on the Linux host (mingw-w64):
//   x86_64-w64-mingw32-g++ -O2 -static -o dupshot.exe dupshot.cpp -ld3d11 -ldxgi -lole32 -lwindowscodecs -luuid
//
// Run in the interactive session (schtasks /IT; session 0 has no desktop to duplicate):
//   dupshot.exe [out.png] [output=N] [adapter=N] [wait=MS]
//     out.png    output path (default dupshot.png in the current directory; with several outputs
//                _<n> is inserted before the extension)
//     output=N   only output N of the adapter (default: every output)
//     adapter=N  DXGI adapter index (default: the first adapter with an output attached)
//     wait=MS    how long to wait for the first frame (default 1000)
// Exit code 0 when every duplicated output was written, 1 otherwise.

#define COBJMACROS
#include <windows.h>
#include <d3d11.h>
#include <dxgi1_2.h>
#include <wincodec.h>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>

static void fail(const char* step, HRESULT hr) {
    std::printf("DUPSHOT fail step=%s hr=0x%08lx\n", step, (unsigned long)hr);
}

static bool write_png(const std::wstring& path, const BYTE* bgra, UINT w, UINT h, UINT pitch) {
    IWICImagingFactory* f = nullptr;
    HRESULT hr = CoCreateInstance(CLSID_WICImagingFactory, nullptr, CLSCTX_INPROC_SERVER,
                                  IID_IWICImagingFactory, (void**)&f);
    if (FAILED(hr)) { fail("wic_factory", hr); return false; }
    IWICStream* s = nullptr;
    IWICBitmapEncoder* enc = nullptr;
    IWICBitmapFrameEncode* fr = nullptr;
    bool ok = false;
    do {
        if (FAILED(hr = f->CreateStream(&s))) { fail("wic_stream", hr); break; }
        if (FAILED(hr = s->InitializeFromFilename(path.c_str(), GENERIC_WRITE))) { fail("wic_open", hr); break; }
        if (FAILED(hr = f->CreateEncoder(GUID_ContainerFormatPng, nullptr, &enc))) { fail("wic_encoder", hr); break; }
        if (FAILED(hr = enc->Initialize(s, WICBitmapEncoderNoCache))) { fail("wic_init", hr); break; }
        if (FAILED(hr = enc->CreateNewFrame(&fr, nullptr))) { fail("wic_frame", hr); break; }
        if (FAILED(hr = fr->Initialize(nullptr))) { fail("wic_frame_init", hr); break; }
        if (FAILED(hr = fr->SetSize(w, h))) { fail("wic_size", hr); break; }
        // BGRX: the desktop's alpha byte is undefined, so the PNG has no alpha channel.
        WICPixelFormatGUID fmt = GUID_WICPixelFormat32bppBGR;
        if (FAILED(hr = fr->SetPixelFormat(&fmt))) { fail("wic_format", hr); break; }
        if (FAILED(hr = fr->WritePixels(h, pitch, pitch * h, const_cast<BYTE*>(bgra)))) { fail("wic_write", hr); break; }
        if (FAILED(hr = fr->Commit())) { fail("wic_commit", hr); break; }
        if (FAILED(hr = enc->Commit())) { fail("wic_enc_commit", hr); break; }
        ok = true;
    } while (false);
    if (fr) fr->Release();
    if (enc) enc->Release();
    if (s) s->Release();
    f->Release();
    return ok;
}

static std::wstring widen(const std::string& s) {
    int n = MultiByteToWideChar(CP_ACP, 0, s.c_str(), -1, nullptr, 0);
    std::wstring w(n > 0 ? n - 1 : 0, L'\0');
    if (n > 0) MultiByteToWideChar(CP_ACP, 0, s.c_str(), -1, &w[0], n);
    return w;
}

static std::string path_for(const std::string& base, int idx, bool several) {
    if (!several) return base;
    size_t dot = base.find_last_of('.');
    std::string suffix = "_" + std::to_string(idx);
    if (dot == std::string::npos) return base + suffix;
    return base.substr(0, dot) + suffix + base.substr(dot);
}

static bool shoot(IDXGIAdapter1* adapter, IDXGIOutput* out, int idx, const std::string& path, DWORD wait_ms) {
    IDXGIOutput1* out1 = nullptr;
    HRESULT hr = out->QueryInterface(IID_IDXGIOutput1, (void**)&out1);
    if (FAILED(hr)) { fail("output1", hr); return false; }
    ID3D11Device* dev = nullptr;
    ID3D11DeviceContext* ctx = nullptr;
    D3D_FEATURE_LEVEL fl;
    hr = D3D11CreateDevice(adapter, D3D_DRIVER_TYPE_UNKNOWN, nullptr, 0, nullptr, 0, D3D11_SDK_VERSION, &dev, &fl, &ctx);
    if (FAILED(hr)) { fail("d3d11_device", hr); out1->Release(); return false; }
    IDXGIOutputDuplication* dup = nullptr;
    bool ok = false;
    do {
        if (FAILED(hr = out1->DuplicateOutput(dev, &dup))) { fail("duplicate_output", hr); break; }
        // The first frame after DuplicateOutput carries the whole current desktop image; wait up
        // to `wait_ms` for it.
        IDXGIResource* res = nullptr;
        DXGI_OUTDUPL_FRAME_INFO fi;
        DWORD waited = 0;
        while (true) {
            hr = dup->AcquireNextFrame(100, &fi, &res);
            if (hr == DXGI_ERROR_WAIT_TIMEOUT && (waited += 100) < wait_ms) continue;
            if (FAILED(hr)) { fail(hr == DXGI_ERROR_WAIT_TIMEOUT ? "acquire_timeout" : "acquire", hr); res = nullptr; }
            break;
        }
        if (!res) break;
        ID3D11Texture2D* tex = nullptr;
        hr = res->QueryInterface(IID_ID3D11Texture2D, (void**)&tex);
        res->Release();
        if (FAILED(hr)) { fail("frame_texture", hr); dup->ReleaseFrame(); break; }
        D3D11_TEXTURE2D_DESC td;
        tex->GetDesc(&td);
        D3D11_TEXTURE2D_DESC sd = td;
        sd.Usage = D3D11_USAGE_STAGING;
        sd.BindFlags = 0;
        sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
        sd.MiscFlags = 0;
        sd.MipLevels = 1;
        sd.ArraySize = 1;
        ID3D11Texture2D* st = nullptr;
        hr = dev->CreateTexture2D(&sd, nullptr, &st);
        if (FAILED(hr)) { fail("staging", hr); tex->Release(); dup->ReleaseFrame(); break; }
        ctx->CopyResource(st, tex);
        tex->Release();
        D3D11_MAPPED_SUBRESOURCE m;
        hr = ctx->Map(st, 0, D3D11_MAP_READ, 0, &m);
        if (FAILED(hr)) { fail("map", hr); st->Release(); dup->ReleaseFrame(); break; }
        if (td.Format != DXGI_FORMAT_B8G8R8A8_UNORM && td.Format != DXGI_FORMAT_B8G8R8A8_UNORM_SRGB) {
            std::printf("DUPSHOT fail step=format format=%d size=%ux%u\n", (int)td.Format, td.Width, td.Height);
        } else {
            unsigned long long nonblack = 0;
            const BYTE* p = (const BYTE*)m.pData;
            for (UINT y = 0; y < td.Height; y++) {
                const BYTE* row = p + (size_t)y * m.RowPitch;
                for (UINT x = 0; x < td.Width; x++) {
                    const BYTE* px = row + 4 * x;
                    if (px[0] > 8 || px[1] > 8 || px[2] > 8) nonblack++;
                }
            }
            double frac = (double)nonblack / ((double)td.Width * td.Height);
            if (write_png(widen(path), p, td.Width, td.Height, m.RowPitch)) {
                char full[MAX_PATH];
                if (!GetFullPathNameA(path.c_str(), MAX_PATH, full, nullptr)) std::strcpy(full, path.c_str());
                std::printf("DUPSHOT ok output=%d path=%s size=%ux%u format=%d nonblack=%.4f frames=%u present=%lld\n",
                            idx, full, td.Width, td.Height, (int)td.Format, frac, fi.AccumulatedFrames,
                            (long long)fi.LastPresentTime.QuadPart);
                ok = true;
            }
        }
        ctx->Unmap(st, 0);
        st->Release();
        dup->ReleaseFrame();
    } while (false);
    if (dup) dup->Release();
    ctx->Release();
    dev->Release();
    out1->Release();
    return ok;
}

int main(int argc, char** argv) {
    std::string path = "dupshot.png";
    int want_output = -1, want_adapter = -1;
    DWORD wait_ms = 1000;
    for (int i = 1; i < argc; i++) {
        if (!std::strncmp(argv[i], "output=", 7)) want_output = std::atoi(argv[i] + 7);
        else if (!std::strncmp(argv[i], "adapter=", 8)) want_adapter = std::atoi(argv[i] + 8);
        else if (!std::strncmp(argv[i], "wait=", 5)) wait_ms = (DWORD)std::atoi(argv[i] + 5);
        else path = argv[i];
    }
    // Per-monitor DPI awareness, so the duplicated size is the real mode.
    HMODULE user32 = GetModuleHandleA("user32.dll");
    typedef BOOL(WINAPI * SetCtx)(HANDLE);
    if (SetCtx f = user32 ? (SetCtx)GetProcAddress(user32, "SetProcessDpiAwarenessContext") : nullptr)
        f((HANDLE)-4);  // DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2
    HRESULT hr = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    if (FAILED(hr)) { fail("coinit", hr); return 1; }
    IDXGIFactory1* fac = nullptr;
    hr = CreateDXGIFactory1(IID_IDXGIFactory1, (void**)&fac);
    if (FAILED(hr)) { fail("factory", hr); return 1; }
    int done = 0, failed = 0;
    for (UINT a = 0;; a++) {
        IDXGIAdapter1* ad = nullptr;
        if (fac->EnumAdapters1(a, &ad) == DXGI_ERROR_NOT_FOUND) break;
        if (want_adapter >= 0 && (int)a != want_adapter) { ad->Release(); continue; }
        DXGI_ADAPTER_DESC1 desc;
        ad->GetDesc1(&desc);
        std::vector<IDXGIOutput*> outs;
        for (UINT o = 0;; o++) {
            IDXGIOutput* out = nullptr;
            if (ad->EnumOutputs(o, &out) == DXGI_ERROR_NOT_FOUND) break;
            outs.push_back(out);
        }
        if (!outs.empty()) {
            std::printf("DUPSHOT adapter=%u name=%ls outputs=%zu\n", a, desc.Description, outs.size());
            bool several = want_output < 0 && outs.size() > 1;
            for (size_t o = 0; o < outs.size(); o++) {
                if (want_output < 0 || (int)o == want_output) {
                    if (shoot(ad, outs[o], (int)o, path_for(path, (int)o, several), wait_ms)) done++;
                    else failed++;
                }
            }
        }
        for (auto* out : outs) out->Release();
        ad->Release();
        if (done + failed > 0 && want_adapter < 0) break;
    }
    fac->Release();
    CoUninitialize();
    if (done + failed == 0) {
        std::printf("DUPSHOT fail step=no_output hr=0x00000000\n");
        return 1;
    }
    return failed == 0 ? 0 : 1;
}
