#include <windows.h>
#include <d3d9.h>
#include <cstdio>

// D3D9 on the Helios adapter: the adapter answers capability queries, then a
// windowed device clears a render target that is read back on the CPU. The
// UMD registers no D3D9 driver, so d3d9.dll runs the device through D3D9On12
// on the D3D12 UMD; the probe reports which path it got. Odd dimensions
// exercise the readback pitch.

static const UINT kWidth = 31;
static const UINT kHeight = 17;

static bool check(const char *what, HRESULT hr) {
    if (FAILED(hr)) std::fprintf(stderr, "D3D9 %s failed: 0x%08lx.\n", what, static_cast<unsigned long>(hr));
    return SUCCEEDED(hr);
}

// What applications ask before they create a device. Every one of these
// failed with D3DERR_NOTAVAILABLE while the adapter registered a D3D9 UMD
// path without a D3D9 DDI; engines report that as "required multisample is
// not supported" or "can't set video mode".
static bool check_adapter(IDirect3D9 *d3d) {
    D3DCAPS9 caps = {};
    if (!check("GetDeviceCaps", d3d->GetDeviceCaps(D3DADAPTER_DEFAULT, D3DDEVTYPE_HAL, &caps))) return false;
    if (caps.VertexShaderVersion < D3DVS_VERSION(3, 0) || caps.PixelShaderVersion < D3DPS_VERSION(3, 0)) {
        std::fprintf(stderr, "D3D9 caps report vs %lx / ps %lx, expected shader model 3.\n",
                     static_cast<unsigned long>(caps.VertexShaderVersion), static_cast<unsigned long>(caps.PixelShaderVersion));
        return false;
    }
    bool ok = true;
    for (BOOL windowed = FALSE; windowed <= TRUE; ++windowed) {
        ok &= check("CheckDeviceType", d3d->CheckDeviceType(D3DADAPTER_DEFAULT, D3DDEVTYPE_HAL, D3DFMT_X8R8G8B8, D3DFMT_A8R8G8B8, windowed));
        DWORD quality = 0;
        ok &= check("CheckDeviceMultiSampleType(NONE)",
                    d3d->CheckDeviceMultiSampleType(D3DADAPTER_DEFAULT, D3DDEVTYPE_HAL, D3DFMT_A8R8G8B8, windowed,
                                                    D3DMULTISAMPLE_NONE, &quality));
        ok &= check("CheckDeviceMultiSampleType(NONE, no quality)",
                    d3d->CheckDeviceMultiSampleType(D3DADAPTER_DEFAULT, D3DDEVTYPE_HAL, D3DFMT_A8R8G8B8, windowed,
                                                    D3DMULTISAMPLE_NONE, nullptr));
        ok &= check("CheckDeviceMultiSampleType(4x)",
                    d3d->CheckDeviceMultiSampleType(D3DADAPTER_DEFAULT, D3DDEVTYPE_HAL, D3DFMT_A8R8G8B8, windowed,
                                                    D3DMULTISAMPLE_4_SAMPLES, nullptr));
    }
    ok &= check("CheckDeviceFormat(A8R8G8B8 render target)",
                d3d->CheckDeviceFormat(D3DADAPTER_DEFAULT, D3DDEVTYPE_HAL, D3DFMT_X8R8G8B8, D3DUSAGE_RENDERTARGET,
                                       D3DRTYPE_TEXTURE, D3DFMT_A8R8G8B8));
    ok &= check("CheckDeviceFormat(DXT5 texture)",
                d3d->CheckDeviceFormat(D3DADAPTER_DEFAULT, D3DDEVTYPE_HAL, D3DFMT_X8R8G8B8, 0, D3DRTYPE_TEXTURE, D3DFMT_DXT5));
    ok &= check("CheckDeviceFormat(D24S8 depth)",
                d3d->CheckDeviceFormat(D3DADAPTER_DEFAULT, D3DDEVTYPE_HAL, D3DFMT_X8R8G8B8, D3DUSAGE_DEPTHSTENCIL,
                                       D3DRTYPE_SURFACE, D3DFMT_D24S8));
    ok &= check("CheckDepthStencilMatch(D24S8)",
                d3d->CheckDepthStencilMatch(D3DADAPTER_DEFAULT, D3DDEVTYPE_HAL, D3DFMT_X8R8G8B8, D3DFMT_A8R8G8B8, D3DFMT_D24S8));
    return ok;
}

static bool check_readback(IDirect3DDevice9 *device) {
    IDirect3DSurface9 *target = nullptr;
    IDirect3DSurface9 *staging = nullptr;
    bool valid = false;
    HRESULT hr = device->CreateRenderTarget(kWidth, kHeight, D3DFMT_A8R8G8B8, D3DMULTISAMPLE_NONE, 0, FALSE, &target, nullptr);
    if (SUCCEEDED(hr)) hr = device->CreateOffscreenPlainSurface(kWidth, kHeight, D3DFMT_A8R8G8B8, D3DPOOL_SYSTEMMEM, &staging, nullptr);
    if (SUCCEEDED(hr)) hr = device->SetRenderTarget(0, target);
    if (SUCCEEDED(hr)) hr = device->Clear(0, nullptr, D3DCLEAR_TARGET, D3DCOLOR_ARGB(255, 255, 0, 0), 1.0f, 0);
    if (SUCCEEDED(hr)) hr = device->GetRenderTargetData(target, staging);
    D3DLOCKED_RECT locked = {};
    if (SUCCEEDED(hr)) hr = staging->LockRect(&locked, nullptr, D3DLOCK_READONLY);
    if (SUCCEEDED(hr)) {
        valid = locked.pBits && locked.Pitch >= static_cast<INT>(kWidth * 4);
        if (!valid) std::fprintf(stderr, "D3D9 LockRect returned an invalid pointer or pitch.\n");
        for (UINT y = 0; valid && y < kHeight; ++y) {
            const auto *row = static_cast<const unsigned char *>(locked.pBits) + y * locked.Pitch;
            for (UINT x = 0; x < kWidth; ++x) {
                const auto *pixel = row + x * 4;
                if (pixel[0] != 0 || pixel[1] != 0 || pixel[2] != 255 || pixel[3] != 255) {
                    std::fprintf(stderr, "D3D9 readback mismatch at (%u,%u): %02x %02x %02x %02x.\n",
                                 x, y, pixel[0], pixel[1], pixel[2], pixel[3]);
                    valid = false;
                    break;
                }
            }
        }
        staging->UnlockRect();
    }
    if (FAILED(hr)) std::fprintf(stderr, "D3D9 clear/readback failed: 0x%08lx.\n", static_cast<unsigned long>(hr));
    if (staging) staging->Release();
    if (target) target->Release();
    if (valid) std::printf("Direct3D 9 clear/readback: all %u pixels match.\n", kWidth * kHeight);
    return valid;
}

int main() {
    IDirect3D9 *d3d = Direct3DCreate9(D3D_SDK_VERSION);
    if (!d3d) {
        std::fprintf(stderr, "Direct3DCreate9 failed.\n");
        return 1;
    }
    D3DADAPTER_IDENTIFIER9 id = {};
    if (SUCCEEDED(d3d->GetAdapterIdentifier(D3DADAPTER_DEFAULT, 0, &id))) {
        std::printf("Direct3D 9 adapter: %s (%04lx:%04lx)\n", id.Description,
                    static_cast<unsigned long>(id.VendorId), static_cast<unsigned long>(id.DeviceId));
    }
    bool ok = check_adapter(d3d);

    WNDCLASSW window_class = {};
    window_class.lpfnWndProc = DefWindowProcW;
    window_class.hInstance = GetModuleHandleW(nullptr);
    window_class.lpszClassName = L"HeliosD3D9Smoke";
    RegisterClassW(&window_class);
    HWND window = CreateWindowExW(0, window_class.lpszClassName, L"Helios D3D9 smoke", WS_OVERLAPPEDWINDOW, 0, 0,
                                  kWidth, kHeight, nullptr, nullptr, window_class.hInstance, nullptr);
    D3DPRESENT_PARAMETERS present = {};
    present.BackBufferWidth = kWidth;
    present.BackBufferHeight = kHeight;
    present.BackBufferFormat = D3DFMT_A8R8G8B8;
    present.BackBufferCount = 1;
    present.SwapEffect = D3DSWAPEFFECT_DISCARD;
    present.hDeviceWindow = window;
    present.Windowed = TRUE;
    present.EnableAutoDepthStencil = TRUE;
    present.AutoDepthStencilFormat = D3DFMT_D24S8;
    present.PresentationInterval = D3DPRESENT_INTERVAL_IMMEDIATE;
    IDirect3DDevice9 *device = nullptr;
    if (!window) {
        std::fprintf(stderr, "D3D9 probe window creation failed: %lu.\n", GetLastError());
        ok = false;
    } else if (check("CreateDevice", d3d->CreateDevice(D3DADAPTER_DEFAULT, D3DDEVTYPE_HAL, window,
                                                       D3DCREATE_HARDWARE_VERTEXPROCESSING | D3DCREATE_FPU_PRESERVE,
                                                       &present, &device))) {
        // IID_IDirect3DDevice9On12 (d3d9.h, Windows 10 SDK 19041+), spelled out
        // so older SDK headers build the probe too.
        static const GUID kDevice9On12 = {0xe7fda234, 0xb589, 0x4049, {0x94, 0x0d, 0x88, 0x78, 0x97, 0x75, 0x31, 0xc8}};
        IUnknown *on12 = nullptr;
        const bool via_9on12 = SUCCEEDED(device->QueryInterface(kDevice9On12, reinterpret_cast<void **>(&on12)));
        if (on12) on12->Release();
        std::printf("Direct3D 9 device: %s.\n", via_9on12 ? "D3D9On12 on the D3D12 UMD" : "native D3D9 driver");
        ok &= check_readback(device);
        ok &= check("Present", device->Present(nullptr, nullptr, nullptr, nullptr));
        device->Release();
    } else {
        ok = false;
    }
    if (window) DestroyWindow(window);
    d3d->Release();
    if (ok) std::printf("Direct3D 9 smoke test passed.\n");
    return ok ? 0 : 1;
}
