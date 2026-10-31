/*
 * d3d11test: Direct3D 11 smoke test.
 *
 * Loads dxgi.dll / d3d11.dll (and d3dcompiler_47.dll) at run time, picks the
 * NVIDIA adapter (vendor 0x10DE), creates a feature level 11_0+ device, then:
 *   1. clears a render target, copies it to a staging texture, maps and checks it
 *   2. compiles a cs_5_0 compute shader with D3DCompile, writes a UAV buffer,
 *      copies it to a staging buffer and checks it (skipped with a warning if
 *      d3dcompiler_47.dll is missing)
 *   3. presents 30 frames to a window through a flip-model swap chain, switches
 *      to fullscreen for 30 frames and back (a failed fullscreen switch is only
 *      a warning), then destroys everything
 * Options: --no-present  skip step 3
 * Every GPU wait uses an event query polled with a 5 s timeout.
 * Prints step-by-step progress and a final "PASS" or "FAIL: <reason>" line.
 */
#define COBJMACROS
#include "common.h"
#include <d3d11.h>
#include <dxgi1_2.h>
#include <d3dcompiler.h>

#define TEX_SIZE 256u
#define CS_COUNT 4096u
#define FRAMES 30

typedef HRESULT(WINAPI *PFN_CreateDXGIFactory1)(REFIID, void **);

static ID3D11Device *dev;
static ID3D11DeviceContext *ctx;

#define HR_CHECK(call)                                                   \
    do {                                                                 \
        HRESULT hr_ = (call);                                            \
        if (FAILED(hr_))                                                 \
            return fail("%s failed: hr 0x%08lx", #call, (unsigned long)hr_); \
    } while (0)

/* Waits for all work submitted so far using an event query (5 s timeout). */
static int gpu_wait(const char *what)
{
    D3D11_QUERY_DESC qd = {D3D11_QUERY_EVENT, 0};
    ID3D11Query *q;
    BOOL done = FALSE;
    HRESULT hr;
    double t0 = now_ms();

    HR_CHECK(ID3D11Device_CreateQuery(dev, &qd, &q));
    ID3D11DeviceContext_End(ctx, (ID3D11Asynchronous *)q);
    ID3D11DeviceContext_Flush(ctx);
    for (;;) {
        hr = ID3D11DeviceContext_GetData(ctx, (ID3D11Asynchronous *)q, &done, sizeof(done), 0);
        if (hr == S_OK && done)
            break;
        if (FAILED(hr)) {
            ID3D11Query_Release(q);
            return fail("%s: GetData failed hr 0x%08lx, device removed reason 0x%08lx", what,
                        (unsigned long)hr, (unsigned long)ID3D11Device_GetDeviceRemovedReason(dev));
        }
        if (now_ms() - t0 > WAIT_TIMEOUT_MS) {
            ID3D11Query_Release(q);
            return fail("%s: GPU did not finish within %u ms", what, WAIT_TIMEOUT_MS);
        }
        Sleep(1);
    }
    ID3D11Query_Release(q);
    logmsg("%s: GPU idle after %.2f ms", what, now_ms() - t0);
    return 0;
}

static int test_clear(void)
{
    D3D11_TEXTURE2D_DESC td;
    ID3D11Texture2D *rt, *staging;
    ID3D11RenderTargetView *rtv;
    D3D11_MAPPED_SUBRESOURCE map;
    static const float color[4] = {0.2f, 0.4f, 0.6f, 1.0f};
    static const unsigned char want[4] = {51, 102, 153, 255};
    unsigned x, y, c, bad = 0;

    logmsg("clear test: %ux%u R8G8B8A8_UNORM render target", TEX_SIZE, TEX_SIZE);
    memset(&td, 0, sizeof(td));
    td.Width = td.Height = TEX_SIZE;
    td.MipLevels = td.ArraySize = 1;
    td.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    td.SampleDesc.Count = 1;
    td.Usage = D3D11_USAGE_DEFAULT;
    td.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
    HR_CHECK(ID3D11Device_CreateTexture2D(dev, &td, NULL, &rt));
    HR_CHECK(ID3D11Device_CreateRenderTargetView(dev, (ID3D11Resource *)rt, NULL, &rtv));
    td.Usage = D3D11_USAGE_STAGING;
    td.BindFlags = 0;
    td.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    HR_CHECK(ID3D11Device_CreateTexture2D(dev, &td, NULL, &staging));

    ID3D11DeviceContext_ClearRenderTargetView(ctx, rtv, color);
    ID3D11DeviceContext_CopyResource(ctx, (ID3D11Resource *)staging, (ID3D11Resource *)rt);
    if (gpu_wait("clear+copy"))
        return 1;
    HR_CHECK(ID3D11DeviceContext_Map(ctx, (ID3D11Resource *)staging, 0, D3D11_MAP_READ, 0, &map));
    for (y = 0; y < TEX_SIZE; y++) {
        const unsigned char *row = (const unsigned char *)map.pData + (size_t)y * map.RowPitch;
        for (x = 0; x < TEX_SIZE; x++)
            for (c = 0; c < 4; c++) {
                int d = (int)row[x * 4 + c] - (int)want[c];
                if (d < -1 || d > 1) {
                    if (bad < 5)
                        logmsg("mismatch at (%u,%u) channel %u: got %u want %u", x, y, c,
                               row[x * 4 + c], want[c]);
                    bad++;
                }
            }
    }
    ID3D11DeviceContext_Unmap(ctx, (ID3D11Resource *)staging, 0);
    ID3D11Texture2D_Release(staging);
    ID3D11RenderTargetView_Release(rtv);
    ID3D11Texture2D_Release(rt);
    if (bad)
        return fail("clear test: %u channel values wrong (row pitch %u)", bad, map.RowPitch);
    logmsg("clear test: all %u pixels correct (row pitch %u)", TEX_SIZE * TEX_SIZE, map.RowPitch);
    return 0;
}

static const char cs_hlsl[] =
    "RWStructuredBuffer<uint> outb : register(u0);\n"
    "[numthreads(64, 1, 1)]\n"
    "void main(uint3 id : SV_DispatchThreadID)\n"
    "{\n"
    "    outb[id.x] = id.x * 3u + 7u;\n"
    "}\n";

static int test_compute(void)
{
    HMODULE lib;
    pD3DCompile compile;
    ID3DBlob *code = NULL, *errors = NULL;
    HRESULT hr;
    ID3D11ComputeShader *cs;
    D3D11_BUFFER_DESC bd;
    D3D11_UNORDERED_ACCESS_VIEW_DESC ud;
    ID3D11Buffer *buf, *staging;
    ID3D11UnorderedAccessView *uav, *null_uav = NULL;
    D3D11_MAPPED_SUBRESOURCE map;
    unsigned i, bad = 0;
    const unsigned *p;

    logmsg("compute test: loading d3dcompiler_47.dll");
    lib = LoadLibraryA("d3dcompiler_47.dll");
    compile = (pD3DCompile)load_sym(lib, "D3DCompile");
    if (!compile) {
        logmsg("WARNING: d3dcompiler_47.dll / D3DCompile not available, skipping compute test");
        return 0;
    }
    hr = compile(cs_hlsl, sizeof(cs_hlsl) - 1, "cs.hlsl", NULL, NULL, "main", "cs_5_0",
                 D3DCOMPILE_OPTIMIZATION_LEVEL3, 0, &code, &errors);
    if (FAILED(hr))
        return fail("D3DCompile failed hr 0x%08lx: %s", (unsigned long)hr,
                    errors ? (const char *)ID3D10Blob_GetBufferPointer(errors) : "(no log)");
    if (errors)
        ID3D10Blob_Release(errors);
    logmsg("compute test: cs_5_0 compiled, %u bytes DXBC", (unsigned)ID3D10Blob_GetBufferSize(code));
    HR_CHECK(ID3D11Device_CreateComputeShader(dev, ID3D10Blob_GetBufferPointer(code),
                                              ID3D10Blob_GetBufferSize(code), NULL, &cs));
    ID3D10Blob_Release(code);

    memset(&bd, 0, sizeof(bd));
    bd.ByteWidth = CS_COUNT * 4;
    bd.Usage = D3D11_USAGE_DEFAULT;
    bd.BindFlags = D3D11_BIND_UNORDERED_ACCESS;
    bd.MiscFlags = D3D11_RESOURCE_MISC_BUFFER_STRUCTURED;
    bd.StructureByteStride = 4;
    HR_CHECK(ID3D11Device_CreateBuffer(dev, &bd, NULL, &buf));
    memset(&ud, 0, sizeof(ud));
    ud.Format = DXGI_FORMAT_UNKNOWN;
    ud.ViewDimension = D3D11_UAV_DIMENSION_BUFFER;
    ud.Buffer.NumElements = CS_COUNT;
    HR_CHECK(ID3D11Device_CreateUnorderedAccessView(dev, (ID3D11Resource *)buf, &ud, &uav));
    bd.Usage = D3D11_USAGE_STAGING;
    bd.BindFlags = 0;
    bd.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    bd.MiscFlags = 0;
    bd.StructureByteStride = 0;
    HR_CHECK(ID3D11Device_CreateBuffer(dev, &bd, NULL, &staging));

    logmsg("compute test: Dispatch(%u, 1, 1)", CS_COUNT / 64);
    ID3D11DeviceContext_CSSetShader(ctx, cs, NULL, 0);
    ID3D11DeviceContext_CSSetUnorderedAccessViews(ctx, 0, 1, &uav, NULL);
    ID3D11DeviceContext_Dispatch(ctx, CS_COUNT / 64, 1, 1);
    ID3D11DeviceContext_CSSetUnorderedAccessViews(ctx, 0, 1, &null_uav, NULL);
    ID3D11DeviceContext_CSSetShader(ctx, NULL, NULL, 0);
    ID3D11DeviceContext_CopyResource(ctx, (ID3D11Resource *)staging, (ID3D11Resource *)buf);
    if (gpu_wait("dispatch+copy"))
        return 1;
    HR_CHECK(ID3D11DeviceContext_Map(ctx, (ID3D11Resource *)staging, 0, D3D11_MAP_READ, 0, &map));
    p = (const unsigned *)map.pData;
    for (i = 0; i < CS_COUNT; i++)
        if (p[i] != i * 3u + 7u) {
            if (bad < 5)
                logmsg("mismatch at %u: got %u want %u", i, p[i], i * 3u + 7u);
            bad++;
        }
    ID3D11DeviceContext_Unmap(ctx, (ID3D11Resource *)staging, 0);
    ID3D11Buffer_Release(staging);
    ID3D11UnorderedAccessView_Release(uav);
    ID3D11Buffer_Release(buf);
    ID3D11ComputeShader_Release(cs);
    if (bad)
        return fail("compute test: %u of %u values wrong", bad, CS_COUNT);
    logmsg("compute test: all %u values correct", CS_COUNT);
    return 0;
}

static LRESULT CALLBACK wnd_proc(HWND hwnd, UINT msg, WPARAM wp, LPARAM lp)
{
    return DefWindowProcW(hwnd, msg, wp, lp);
}

static void pump_messages(void)
{
    MSG msg;
    int n = 0;
    while (n++ < 100 && PeekMessageW(&msg, NULL, 0, 0, PM_REMOVE)) {
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

static int present_frames(IDXGISwapChain1 *sc, int first, int count, const char *what)
{
    int f;
    double t0 = now_ms();
    for (f = first; f < first + count; f++) {
        ID3D11Texture2D *bb;
        ID3D11RenderTargetView *rtv;
        float t = (float)(f % 60) / 59.0f;
        float color[4];
        HRESULT hr;
        color[0] = t;
        color[1] = 1.0f - t;
        color[2] = (f & 1) ? 0.5f : 0.25f;
        color[3] = 1.0f;
        pump_messages();
        HR_CHECK(IDXGISwapChain1_GetBuffer(sc, 0, &IID_ID3D11Texture2D, (void **)&bb));
        HR_CHECK(ID3D11Device_CreateRenderTargetView(dev, (ID3D11Resource *)bb, NULL, &rtv));
        ID3D11DeviceContext_OMSetRenderTargets(ctx, 1, &rtv, NULL);
        ID3D11DeviceContext_ClearRenderTargetView(ctx, rtv, color);
        ID3D11DeviceContext_OMSetRenderTargets(ctx, 0, NULL, NULL);
        ID3D11RenderTargetView_Release(rtv);
        ID3D11Texture2D_Release(bb);
        hr = IDXGISwapChain1_Present(sc, 1, 0);
        if (FAILED(hr))
            return fail("%s: Present frame %d failed hr 0x%08lx (device removed reason 0x%08lx)", what,
                        f, (unsigned long)hr,
                        (unsigned long)ID3D11Device_GetDeviceRemovedReason(dev));
        if (hr != S_OK && f == first)
            logmsg("%s: Present returned status 0x%08lx", what, (unsigned long)hr);
    }
    if (gpu_wait(what))
        return 1;
    logmsg("%s: %d frames in %.1f ms", what, count, now_ms() - t0);
    return 0;
}

/* Resizes the back buffers to the new window/output size (no references are held). */
static void resize_buffers(IDXGISwapChain1 *sc, const char *what)
{
    DXGI_SWAP_CHAIN_DESC1 d;
    HRESULT hr;
    ID3D11DeviceContext_ClearState(ctx);
    ID3D11DeviceContext_Flush(ctx);
    hr = IDXGISwapChain1_ResizeBuffers(sc, 0, 0, 0, DXGI_FORMAT_UNKNOWN,
                                       DXGI_SWAP_CHAIN_FLAG_ALLOW_MODE_SWITCH);
    if (FAILED(hr)) {
        logmsg("WARNING: ResizeBuffers after %s switch failed hr 0x%08lx", what, (unsigned long)hr);
        return;
    }
    if (SUCCEEDED(IDXGISwapChain1_GetDesc1(sc, &d)))
        logmsg("present test: back buffers now %ux%u", d.Width, d.Height);
}

static int test_present(IDXGIFactory1 *factory1)
{
    WNDCLASSEXW wc;
    HWND hwnd;
    RECT rc = {0, 0, 640, 480};
    IDXGIFactory2 *factory2;
    DXGI_SWAP_CHAIN_DESC1 scd;
    IDXGISwapChain1 *sc;
    HRESULT hr;
    BOOL fs = FALSE;

    logmsg("present test: creating window");
    memset(&wc, 0, sizeof(wc));
    wc.cbSize = sizeof(wc);
    wc.lpfnWndProc = wnd_proc;
    wc.hInstance = GetModuleHandleW(NULL);
    wc.hCursor = LoadCursorW(NULL, (LPCWSTR)IDC_ARROW);
    wc.lpszClassName = L"nvtrace_d3d11test";
    if (!RegisterClassExW(&wc))
        return fail("RegisterClassEx failed, error %lu", (unsigned long)GetLastError());
    AdjustWindowRect(&rc, WS_OVERLAPPEDWINDOW, FALSE);
    hwnd = CreateWindowExW(0, wc.lpszClassName, L"nvtrace d3d11test", WS_OVERLAPPEDWINDOW,
                           CW_USEDEFAULT, CW_USEDEFAULT, rc.right - rc.left, rc.bottom - rc.top,
                           NULL, NULL, wc.hInstance, NULL);
    if (!hwnd)
        return fail("CreateWindowEx failed, error %lu", (unsigned long)GetLastError());
    ShowWindow(hwnd, SW_SHOWNORMAL);
    SetForegroundWindow(hwnd);
    pump_messages();

    HR_CHECK(IDXGIFactory1_QueryInterface(factory1, &IID_IDXGIFactory2, (void **)&factory2));
    memset(&scd, 0, sizeof(scd));
    scd.Width = 640;
    scd.Height = 480;
    scd.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
    scd.SampleDesc.Count = 1;
    scd.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
    scd.BufferCount = 2;
    scd.Scaling = DXGI_SCALING_STRETCH;
    scd.SwapEffect = DXGI_SWAP_EFFECT_FLIP_DISCARD;
    scd.AlphaMode = DXGI_ALPHA_MODE_UNSPECIFIED;
    scd.Flags = DXGI_SWAP_CHAIN_FLAG_ALLOW_MODE_SWITCH;
    logmsg("present test: CreateSwapChainForHwnd (640x480 B8G8R8A8, 2 buffers, FLIP_DISCARD)");
    HR_CHECK(IDXGIFactory2_CreateSwapChainForHwnd(factory2, (IUnknown *)dev, hwnd, &scd, NULL, NULL, &sc));
    IDXGIFactory2_MakeWindowAssociation(factory2, hwnd, DXGI_MWA_NO_ALT_ENTER);

    if (present_frames(sc, 0, FRAMES, "windowed"))
        return 1;

    logmsg("present test: SetFullscreenState(TRUE)");
    hr = IDXGISwapChain1_SetFullscreenState(sc, TRUE, NULL);
    if (FAILED(hr)) {
        logmsg("WARNING: SetFullscreenState(TRUE) failed hr 0x%08lx, skipping fullscreen frames",
               (unsigned long)hr);
    } else {
        pump_messages();
        resize_buffers(sc, "fullscreen");
        IDXGISwapChain1_GetFullscreenState(sc, &fs, NULL);
        logmsg("present test: fullscreen state now %d", (int)fs);
        if (present_frames(sc, FRAMES, FRAMES, "fullscreen"))
            return 1;
        logmsg("present test: SetFullscreenState(FALSE)");
        hr = IDXGISwapChain1_SetFullscreenState(sc, FALSE, NULL);
        if (FAILED(hr))
            logmsg("WARNING: SetFullscreenState(FALSE) failed hr 0x%08lx", (unsigned long)hr);
        pump_messages();
        resize_buffers(sc, "windowed");
        if (present_frames(sc, 2 * FRAMES, 5, "windowed again"))
            return 1;
    }

    logmsg("present test: destroying swap chain and window");
    IDXGISwapChain1_GetFullscreenState(sc, &fs, NULL);
    if (fs)
        IDXGISwapChain1_SetFullscreenState(sc, FALSE, NULL);
    ID3D11DeviceContext_ClearState(ctx);
    ID3D11DeviceContext_Flush(ctx);
    IDXGISwapChain1_Release(sc);
    IDXGIFactory2_Release(factory2);
    DestroyWindow(hwnd);
    pump_messages();
    UnregisterClassW(wc.lpszClassName, wc.hInstance);
    return 0;
}

int main(int argc, char **argv)
{
    HMODULE dxgi_lib, d3d11_lib;
    PFN_CreateDXGIFactory1 create_factory;
    PFN_D3D11_CREATE_DEVICE create_device;
    IDXGIFactory1 *factory;
    IDXGIAdapter1 *adapter, *chosen = NULL;
    DXGI_ADAPTER_DESC1 ad;
    UINT i;
    char name[256];
    static const D3D_FEATURE_LEVEL levels[] = {D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0};
    D3D_FEATURE_LEVEL got;
    HRESULT hr;
    UINT flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;
    int no_present = has_arg(argc, argv, "--no-present");

    common_init("d3d11test");

    logmsg("loading dxgi.dll and d3d11.dll");
    dxgi_lib = LoadLibraryA("dxgi.dll");
    d3d11_lib = LoadLibraryA("d3d11.dll");
    create_factory = (PFN_CreateDXGIFactory1)load_sym(dxgi_lib, "CreateDXGIFactory1");
    create_device = (PFN_D3D11_CREATE_DEVICE)load_sym(d3d11_lib, "D3D11CreateDevice");
    if (!create_factory)
        return fail("dxgi.dll / CreateDXGIFactory1 not available");
    if (!create_device)
        return fail("d3d11.dll / D3D11CreateDevice not available");

    HR_CHECK(create_factory(&IID_IDXGIFactory1, (void **)&factory));
    for (i = 0; IDXGIFactory1_EnumAdapters1(factory, i, &adapter) == S_OK; i++) {
        HR_CHECK(IDXGIAdapter1_GetDesc1(adapter, &ad));
        wide_to_utf8(ad.Description, name, sizeof(name));
        logmsg("adapter %u: %s, vendor 0x%04x device 0x%04x, LUID %08lx:%08lx, %llu MiB VRAM%s", i,
               name, ad.VendorId, ad.DeviceId, (unsigned long)ad.AdapterLuid.HighPart,
               (unsigned long)ad.AdapterLuid.LowPart,
               (unsigned long long)(ad.DedicatedVideoMemory >> 20),
               (ad.Flags & DXGI_ADAPTER_FLAG_SOFTWARE) ? " (software)" : "");
        if (!chosen && ad.VendorId == NV_VENDOR_ID && !(ad.Flags & DXGI_ADAPTER_FLAG_SOFTWARE)) {
            chosen = adapter;
            continue;
        }
        IDXGIAdapter1_Release(adapter);
    }
    if (!chosen) {
        logmsg("WARNING: no NVIDIA (0x10DE) adapter found, falling back to adapter 0");
        HR_CHECK(IDXGIFactory1_EnumAdapters1(factory, 0, &chosen));
    }
    HR_CHECK(IDXGIAdapter1_GetDesc1(chosen, &ad));
    wide_to_utf8(ad.Description, name, sizeof(name));
    logmsg("using adapter %s, LUID %08lx:%08lx", name, (unsigned long)ad.AdapterLuid.HighPart,
           (unsigned long)ad.AdapterLuid.LowPart);

    logmsg("D3D11CreateDevice (feature level 11_1/11_0)");
    hr = create_device((IDXGIAdapter *)chosen, D3D_DRIVER_TYPE_UNKNOWN, NULL, flags, levels, 2,
                       D3D11_SDK_VERSION, &dev, &got, &ctx);
    if (hr == E_INVALIDARG) /* runtimes without 11_1 reject the list */
        hr = create_device((IDXGIAdapter *)chosen, D3D_DRIVER_TYPE_UNKNOWN, NULL, flags, levels + 1, 1,
                           D3D11_SDK_VERSION, &dev, &got, &ctx);
    if (FAILED(hr))
        return fail("D3D11CreateDevice failed: hr 0x%08lx", (unsigned long)hr);
    logmsg("device created, feature level 0x%x", (unsigned)got);

    if (test_clear())
        return 1;
    if (test_compute())
        return 1;
    if (no_present)
        logmsg("--no-present: skipping window/swap chain test");
    else if (test_present(factory))
        return 1;

    hr = ID3D11Device_GetDeviceRemovedReason(dev);
    if (hr != S_OK)
        return fail("device removed, reason 0x%08lx", (unsigned long)hr);
    logmsg("cleanup");
    ID3D11DeviceContext_ClearState(ctx);
    ID3D11DeviceContext_Flush(ctx);
    ID3D11DeviceContext_Release(ctx);
    ID3D11Device_Release(dev);
    IDXGIAdapter1_Release(chosen);
    IDXGIFactory1_Release(factory);
    return pass();
}
