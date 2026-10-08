// dupshot.cpp: a screenshot through DXGI Desktop Duplication, independent of the GDI screen read
// (CopyFromScreen) and of host-side capture.
//
// IDXGIOutput1::DuplicateOutput -> AcquireNextFrame -> CopyResource into a CPU-readable staging
// texture -> PNG (8-bit RGB, own encoder). The mouse pointer is not drawn (duplication delivers it separately; this
// tool ignores it). Prints one line per output:
//   DUPSHOT ok path=<png> size=<w>x<h> format=<dxgi> nonblack=<fraction> frames=<accumulated>
//   DUPSHOT fail step=<what> hr=0x<HRESULT>
// "nonblack" is the fraction of pixels with any of R, G, B above 8.
//
// Build on the Linux host (mingw-w64):
//   x86_64-w64-mingw32-g++ -O2 -static -o dupshot.exe dupshot.cpp -ld3d11 -ldxgi -lole32 -luuid -lgdi32
//
// Run in the interactive session (schtasks /IT; session 0 has no desktop to duplicate):
//   dupshot.exe [out.png] [output=N] [adapter=N] [wait=MS]
//   dupshot.exe selftest [out.png]   (a known ramp through the same conversion and PNG writer)
//     out.png    output path (default dupshot.png in the current directory; with several outputs
//                _<n> is inserted before the extension)
//     output=N   only output N of the adapter (default: every output)
//     adapter=N  DXGI adapter index (default: the first adapter with an output attached)
//     wait=MS    how long to wait for the first frame (default 1000)
// Exit code 0 when every duplicated output was written, 1 otherwise.

#define COBJMACROS
#include <windows.h>
#include <d3d11.h>
#include <dxgi1_6.h>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>
#include <algorithm>
#include <cstdint>

// IID_IDXGIOutput5 (mingw's libdxgi lacks it): 80A07424-AB52-42EB-833C-0C42FD282D98.
static const GUID kIID_IDXGIOutput5 = {0x80a07424, 0xab52, 0x42eb, {0x83, 0x3c, 0x0c, 0x42, 0xfd, 0x28, 0x2d, 0x98}};

static void fail(const char* step, HRESULT hr) {
    std::printf("DUPSHOT fail step=%s hr=0x%08lx\n", step, (unsigned long)hr);
}

// A plain 8-bit RGB PNG, written without WIC: the WIC PNG encoder silently negotiated a different
// pixel format for 32bppBGR input (the first dupshot wrote grayscale, striped images). Stored
// (uncompressed) deflate blocks, so the encoder is a few lines and its output is exact.
static uint32_t crc_table[256];
static void crc_init() {
    for (uint32_t n = 0; n < 256; n++) {
        uint32_t c = n;
        for (int k = 0; k < 8; k++) c = (c & 1) ? 0xEDB88320u ^ (c >> 1) : c >> 1;
        crc_table[n] = c;
    }
}
static uint32_t crc_update(uint32_t c, const uint8_t* b, size_t n) {
    for (size_t i = 0; i < n; i++) c = crc_table[(c ^ b[i]) & 0xff] ^ (c >> 8);
    return c;
}
static void put32be(std::vector<uint8_t>& v, uint32_t x) {
    v.push_back(x >> 24); v.push_back(x >> 16); v.push_back(x >> 8); v.push_back(x);
}
static void chunk(FILE* f, const char* type, const std::vector<uint8_t>& data) {
    std::vector<uint8_t> h;
    put32be(h, (uint32_t)data.size());
    fwrite(h.data(), 1, 4, f);
    uint32_t c = crc_update(0xffffffffu, (const uint8_t*)type, 4);
    c = crc_update(c, data.data(), data.size());
    fwrite(type, 1, 4, f);
    if (!data.empty()) fwrite(data.data(), 1, data.size(), f);
    std::vector<uint8_t> t;
    put32be(t, c ^ 0xffffffffu);
    fwrite(t.data(), 1, 4, f);
}

// rgb: w*3 bytes per row, rows packed.
static bool write_png_rgb(const char* path, const std::vector<uint8_t>& rgb, UINT w, UINT h) {
    crc_init();
    FILE* f = std::fopen(path, "wb");
    if (!f) { fail("png_open", HRESULT_FROM_WIN32(GetLastError())); return false; }
    static const uint8_t sig[8] = {0x89, 'P', 'N', 'G', 0x0d, 0x0a, 0x1a, 0x0a};
    fwrite(sig, 1, 8, f);
    std::vector<uint8_t> ihdr;
    put32be(ihdr, w); put32be(ihdr, h);
    ihdr.push_back(8); ihdr.push_back(2); ihdr.push_back(0); ihdr.push_back(0); ihdr.push_back(0);
    chunk(f, "IHDR", ihdr);
    // Raw scanlines: filter byte 0 + row.
    std::vector<uint8_t> raw;
    raw.reserve((size_t)h * (w * 3 + 1));
    for (UINT y = 0; y < h; y++) {
        raw.push_back(0);
        raw.insert(raw.end(), rgb.begin() + (size_t)y * w * 3, rgb.begin() + (size_t)(y + 1) * w * 3);
    }
    // zlib stream of stored blocks.
    std::vector<uint8_t> z;
    z.reserve(raw.size() + raw.size() / 65535 * 5 + 16);
    z.push_back(0x78); z.push_back(0x01);
    size_t pos = 0;
    do {
        size_t n = std::min<size_t>(65535, raw.size() - pos);
        bool last = pos + n == raw.size();
        z.push_back(last ? 1 : 0);
        z.push_back(n & 0xff); z.push_back(n >> 8);
        z.push_back(~n & 0xff); z.push_back((~n >> 8) & 0xff);
        z.insert(z.end(), raw.begin() + pos, raw.begin() + pos + n);
        pos += n;
    } while (pos < raw.size());
    uint32_t a = 1, b = 0;
    for (uint8_t c : raw) { a = (a + c) % 65521; b = (b + a) % 65521; }
    put32be(z, (b << 16) | a);
    chunk(f, "IDAT", z);
    chunk(f, "IEND", {});
    bool ok = std::fflush(f) == 0;
    std::fclose(f);
    if (!ok) fail("png_write", E_FAIL);
    return ok;
}

// B8G8R8A8 rows `pitch` bytes apart (the mapped subresource's RowPitch) to packed RGB8.
static std::vector<uint8_t> bgra_to_rgb(const BYTE* bgra, UINT w, UINT h, UINT pitch) {
    std::vector<uint8_t> rgb((size_t)w * h * 3);
    for (UINT y = 0; y < h; y++) {
        const BYTE* row = bgra + (size_t)y * pitch;
        uint8_t* out = rgb.data() + (size_t)y * w * 3;
        for (UINT x = 0; x < w; x++) {
            out[3 * x + 0] = row[4 * x + 2];
            out[3 * x + 1] = row[4 * x + 1];
            out[3 * x + 2] = row[4 * x + 0];
        }
    }
    return rgb;
}

// selftest: a known 256x64 image (R = x, G = y*4, B = 255 - x) through the same BGRA->RGB and PNG
// path, plus a check of the converted pixels. Open the PNG: a red-to-blue horizontal ramp, green
// rising downwards, no stripes.
static int selftest(const char* path) {
    const UINT w = 256, h = 64, pitch = w * 4 + 64;  // a padded pitch, as a mapped texture has
    std::vector<BYTE> bgra((size_t)pitch * h, 0xcd);
    for (UINT y = 0; y < h; y++)
        for (UINT x = 0; x < w; x++) {
            BYTE* p = &bgra[(size_t)y * pitch + 4 * x];
            p[0] = (BYTE)(255 - x); p[1] = (BYTE)(y * 4); p[2] = (BYTE)x; p[3] = 0xff;
        }
    std::vector<uint8_t> rgb = bgra_to_rgb(bgra.data(), w, h, pitch);
    int bad = 0;
    for (UINT y = 0; y < h; y++)
        for (UINT x = 0; x < w; x++) {
            const uint8_t* q = &rgb[((size_t)y * w + x) * 3];
            if (q[0] != x || q[1] != y * 4 || q[2] != 255 - x) bad++;
        }
    bool ok = write_png_rgb(path, rgb, w, h);
    std::printf("DUPSHOT selftest path=%s convert_bad=%d png=%s\n", path, bad, ok ? "ok" : "fail");
    return (bad == 0 && ok) ? 0 : 1;
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
        hr = out1->DuplicateOutput(dev, &dup);
        if (FAILED(hr)) {
            fail("duplicate_output", hr);
            // The second entry point (IDXGIOutput5, Windows 10 1703+) takes a different kernel
            // path for the format negotiation; try it before giving up.
            IDXGIOutput5* out5 = nullptr;
            if (SUCCEEDED(out->QueryInterface(kIID_IDXGIOutput5, (void**)&out5))) {
                DXGI_FORMAT fmts[] = {DXGI_FORMAT_B8G8R8A8_UNORM};
                hr = out5->DuplicateOutput1(dev, 0, 1, fmts, &dup);
                out5->Release();
                if (FAILED(hr)) fail("duplicate_output1", hr);
            }
            if (FAILED(hr)) break;
            std::printf("DUPSHOT note duplicate_output1 succeeded where duplicate_output failed\n");
        }
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
            // GDI's view of a few pixels next to the captured ones (informational: equal values
            // mean the capture's channel order and addressing are right).
            {
                HDC dc = GetDC(nullptr);
                const UINT px[3][2] = {{td.Width / 4, td.Height / 4}, {td.Width / 2, td.Height / 2}, {td.Width * 3 / 4, td.Height * 3 / 4}};
                for (auto& q : px) {
                    const BYTE* c = p + (size_t)q[1] * m.RowPitch + 4 * q[0];
                    COLORREF g = dc ? GetPixel(dc, q[0], q[1]) : CLR_INVALID;
                    std::printf("DUPSHOT pixel x=%u y=%u dup_rgb=%02x%02x%02x gdi_rgb=%06lx\n", q[0], q[1], c[2], c[1], c[0],
                                g == CLR_INVALID ? 0xffffffUL : (unsigned long)((GetRValue(g) << 16) | (GetGValue(g) << 8) | GetBValue(g)));
                }
                if (dc) ReleaseDC(nullptr, dc);
            }
            if (write_png_rgb(path.c_str(), bgra_to_rgb(p, td.Width, td.Height, m.RowPitch), td.Width, td.Height)) {
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
    if (argc > 1 && !std::strcmp(argv[1], "selftest"))
        return selftest(argc > 2 ? argv[2] : "dupshot_selftest.png");
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
    std::printf("DUPSHOT pid=%lu\n", (unsigned long)GetCurrentProcessId());
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
