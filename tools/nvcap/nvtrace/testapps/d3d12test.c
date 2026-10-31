/*
 * d3d12test: Direct3D 12 smoke test.
 *
 * Loads dxgi.dll / d3d12.dll (and d3dcompiler_47.dll) at run time, picks the
 * NVIDIA adapter (vendor 0x10DE), creates a device plus a direct and a copy
 * queue and one fence, then:
 *   1. direct queue: clears a committed render target, CopyTextureRegion into
 *      a readback buffer, maps and checks it
 *   2. copy queue: upload buffer -> default buffer -> readback buffer, checks it
 *   3. direct queue: cs_5_0 compute shader (D3DCompile) with a root UAV writes a
 *      buffer, copied to readback and checked (skipped with a warning if
 *      d3dcompiler_47.dll is missing)
 *   4. presents 30 frames to a window through a flip-model swap chain
 *      (IDXGISwapChain3, PRESENT <-> RENDER_TARGET barriers, fence per frame)
 * Every fence wait uses SetEventOnCompletion + WaitForSingleObject(5 s).
 * Options: --no-present  skip step 4
 *          --debug       enable the D3D12 debug layer (needs Graphics Tools)
 * Prints step-by-step progress and a final "PASS" or "FAIL: <reason>" line.
 *
 * Note on the C ABI: methods that return a struct (GetCPUDescriptorHandleForHeapStart,
 * ID3D12Resource::GetDesc, ID3D12Device::GetAdapterLuid, ...) return it through a
 * hidden pointer argument. MinGW's d3d12.h declares the vtable entries with that
 * extra pointer, so they are called through lpVtbl with an out pointer below.
 */
#define COBJMACROS
#include "common.h"
#include <stdint.h>
#include <d3d12.h>
#include <d3d12sdklayers.h>
#include <dxgi1_4.h>
#include <d3dcompiler.h>

#define TEX_SIZE 256u
#define COPY_BYTES (1u << 20)
#define CS_COUNT 4096u
#define FRAMES 30
#define SC_BUFFERS 2

typedef HRESULT(WINAPI *PFN_CreateDXGIFactory1)(REFIID, void **);
typedef HRESULT(WINAPI *PFN_SerializeRootSignature)(const D3D12_ROOT_SIGNATURE_DESC *,
                                                    D3D_ROOT_SIGNATURE_VERSION, ID3DBlob **,
                                                    ID3DBlob **);
typedef HRESULT(WINAPI *PFN_GetDebugInterface)(REFIID, void **);

static ID3D12Device *dev;
static ID3D12CommandQueue *dq, *cq;
static ID3D12CommandAllocator *dalloc;
static ID3D12GraphicsCommandList *dcl;
static ID3D12Fence *fence;
static UINT64 fence_val;
static HANDLE fence_event;
static ID3D12DescriptorHeap *rtv_heap;
static UINT rtv_inc;
static HMODULE d3d12_lib;

#define HR_CHECK(call)                                                   \
    do {                                                                 \
        HRESULT hr_ = (call);                                            \
        if (FAILED(hr_))                                                 \
            return fail("%s failed: hr 0x%08lx", #call, (unsigned long)hr_); \
    } while (0)

/* Struct-returning methods: call through the vtable with the hidden out pointer. */
static D3D12_CPU_DESCRIPTOR_HANDLE rtv_handle(UINT index)
{
    D3D12_CPU_DESCRIPTOR_HANDLE h;
    rtv_heap->lpVtbl->GetCPUDescriptorHandleForHeapStart(rtv_heap, &h);
    h.ptr += (SIZE_T)index * rtv_inc;
    return h;
}

static int wait_value(UINT64 val, const char *what)
{
    if (ID3D12Fence_GetCompletedValue(fence) < val) {
        DWORD w;
        HR_CHECK(ID3D12Fence_SetEventOnCompletion(fence, val, fence_event));
        w = WaitForSingleObject(fence_event, WAIT_TIMEOUT_MS);
        if (w != WAIT_OBJECT_0)
            return fail("%s: fence did not reach %llu within %u ms (wait %lu, completed %llu, "
                        "device removed reason 0x%08lx)",
                        what, (unsigned long long)val, WAIT_TIMEOUT_MS, (unsigned long)w,
                        (unsigned long long)ID3D12Fence_GetCompletedValue(fence),
                        (unsigned long)ID3D12Device_GetDeviceRemovedReason(dev));
    }
    if (ID3D12Fence_GetCompletedValue(fence) == UINT64_MAX)
        return fail("%s: fence value is UINT64_MAX (device removed, reason 0x%08lx)", what,
                    (unsigned long)ID3D12Device_GetDeviceRemovedReason(dev));
    return 0;
}

static int signal_wait(ID3D12CommandQueue *q, const char *what)
{
    double t0 = now_ms();
    UINT64 v = ++fence_val;
    HR_CHECK(ID3D12CommandQueue_Signal(q, fence, v));
    if (wait_value(v, what))
        return 1;
    logmsg("%s: fence %llu reached after %.2f ms", what, (unsigned long long)v, now_ms() - t0);
    return 0;
}

static void execute(ID3D12CommandQueue *q, ID3D12GraphicsCommandList *cl)
{
    ID3D12CommandList *lists[1] = {(ID3D12CommandList *)cl};
    ID3D12CommandQueue_ExecuteCommandLists(q, 1, lists);
}

static void transition(ID3D12GraphicsCommandList *cl, ID3D12Resource *res,
                       D3D12_RESOURCE_STATES before, D3D12_RESOURCE_STATES after)
{
    D3D12_RESOURCE_BARRIER b;
    memset(&b, 0, sizeof(b));
    b.Type = D3D12_RESOURCE_BARRIER_TYPE_TRANSITION;
    b.Transition.pResource = res;
    b.Transition.Subresource = D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES;
    b.Transition.StateBefore = before;
    b.Transition.StateAfter = after;
    ID3D12GraphicsCommandList_ResourceBarrier(cl, 1, &b);
}

static int create_buffer(D3D12_HEAP_TYPE heap, UINT64 size, D3D12_RESOURCE_FLAGS flags,
                         D3D12_RESOURCE_STATES state, ID3D12Resource **out)
{
    D3D12_HEAP_PROPERTIES hp;
    D3D12_RESOURCE_DESC rd;
    memset(&hp, 0, sizeof(hp));
    hp.Type = heap;
    memset(&rd, 0, sizeof(rd));
    rd.Dimension = D3D12_RESOURCE_DIMENSION_BUFFER;
    rd.Width = size;
    rd.Height = 1;
    rd.DepthOrArraySize = 1;
    rd.MipLevels = 1;
    rd.Format = DXGI_FORMAT_UNKNOWN;
    rd.SampleDesc.Count = 1;
    rd.Layout = D3D12_TEXTURE_LAYOUT_ROW_MAJOR;
    rd.Flags = flags;
    HR_CHECK(ID3D12Device_CreateCommittedResource(dev, &hp, D3D12_HEAP_FLAG_NONE, &rd, state, NULL,
                                                  &IID_ID3D12Resource, (void **)out));
    return 0;
}

static int reset_direct_list(ID3D12PipelineState *pso)
{
    HR_CHECK(ID3D12CommandAllocator_Reset(dalloc));
    HR_CHECK(ID3D12GraphicsCommandList_Reset(dcl, dalloc, pso));
    return 0;
}

static int test_clear(void)
{
    D3D12_HEAP_PROPERTIES hp;
    D3D12_RESOURCE_DESC rd;
    D3D12_CLEAR_VALUE cv;
    ID3D12Resource *tex, *rb;
    D3D12_PLACED_SUBRESOURCE_FOOTPRINT fp;
    UINT rows;
    UINT64 row_size, total;
    D3D12_TEXTURE_COPY_LOCATION dst, src;
    D3D12_RANGE range;
    unsigned char *p;
    static const float color[4] = {0.2f, 0.4f, 0.6f, 1.0f};
    static const unsigned char want[4] = {51, 102, 153, 255};
    unsigned x, y, c, bad = 0;

    logmsg("clear test: committed %ux%u R8G8B8A8_UNORM render target", TEX_SIZE, TEX_SIZE);
    memset(&hp, 0, sizeof(hp));
    hp.Type = D3D12_HEAP_TYPE_DEFAULT;
    memset(&rd, 0, sizeof(rd));
    rd.Dimension = D3D12_RESOURCE_DIMENSION_TEXTURE2D;
    rd.Width = TEX_SIZE;
    rd.Height = TEX_SIZE;
    rd.DepthOrArraySize = 1;
    rd.MipLevels = 1;
    rd.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    rd.SampleDesc.Count = 1;
    rd.Layout = D3D12_TEXTURE_LAYOUT_UNKNOWN;
    rd.Flags = D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET;
    memset(&cv, 0, sizeof(cv));
    cv.Format = rd.Format;
    memcpy(cv.Color, color, sizeof(color));
    HR_CHECK(ID3D12Device_CreateCommittedResource(dev, &hp, D3D12_HEAP_FLAG_NONE, &rd,
                                                  D3D12_RESOURCE_STATE_RENDER_TARGET, &cv,
                                                  &IID_ID3D12Resource, (void **)&tex));
    ID3D12Device_CreateRenderTargetView(dev, tex, NULL, rtv_handle(0));

    ID3D12Device_GetCopyableFootprints(dev, &rd, 0, 1, 0, &fp, &rows, &row_size, &total);
    logmsg("clear test: readback footprint row pitch %u, %u rows, %llu bytes", fp.Footprint.RowPitch,
           rows, (unsigned long long)total);
    if (create_buffer(D3D12_HEAP_TYPE_READBACK, total, D3D12_RESOURCE_FLAG_NONE,
                      D3D12_RESOURCE_STATE_COPY_DEST, &rb))
        return 1;

    if (reset_direct_list(NULL))
        return 1;
    ID3D12GraphicsCommandList_ClearRenderTargetView(dcl, rtv_handle(0), color, 0, NULL);
    transition(dcl, tex, D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_COPY_SOURCE);
    memset(&dst, 0, sizeof(dst));
    dst.pResource = rb;
    dst.Type = D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT;
    dst.PlacedFootprint = fp;
    memset(&src, 0, sizeof(src));
    src.pResource = tex;
    src.Type = D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX;
    src.SubresourceIndex = 0;
    ID3D12GraphicsCommandList_CopyTextureRegion(dcl, &dst, 0, 0, 0, &src, NULL);
    HR_CHECK(ID3D12GraphicsCommandList_Close(dcl));
    execute(dq, dcl);
    if (signal_wait(dq, "clear+copy"))
        return 1;

    range.Begin = 0;
    range.End = (SIZE_T)total;
    HR_CHECK(ID3D12Resource_Map(rb, 0, &range, (void **)&p));
    for (y = 0; y < TEX_SIZE; y++) {
        const unsigned char *row = p + fp.Offset + (size_t)y * fp.Footprint.RowPitch;
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
    range.End = 0;
    ID3D12Resource_Unmap(rb, 0, &range);
    ID3D12Resource_Release(rb);
    ID3D12Resource_Release(tex);
    if (bad)
        return fail("clear test: %u channel values wrong", bad);
    logmsg("clear test: all %u pixels correct", TEX_SIZE * TEX_SIZE);
    return 0;
}

static int test_copy_queue(void)
{
    ID3D12CommandAllocator *alloc;
    ID3D12GraphicsCommandList *cl;
    ID3D12Resource *up, *mid, *rb;
    D3D12_RANGE range = {0, 0};
    unsigned *p, i, bad = 0;

    logmsg("copy queue test: upload -> default -> readback, %u bytes", COPY_BYTES);
    if (create_buffer(D3D12_HEAP_TYPE_UPLOAD, COPY_BYTES, D3D12_RESOURCE_FLAG_NONE,
                      D3D12_RESOURCE_STATE_GENERIC_READ, &up) ||
        create_buffer(D3D12_HEAP_TYPE_DEFAULT, COPY_BYTES, D3D12_RESOURCE_FLAG_NONE,
                      D3D12_RESOURCE_STATE_COMMON, &mid) ||
        create_buffer(D3D12_HEAP_TYPE_READBACK, COPY_BYTES, D3D12_RESOURCE_FLAG_NONE,
                      D3D12_RESOURCE_STATE_COPY_DEST, &rb))
        return 1;
    HR_CHECK(ID3D12Resource_Map(up, 0, &range, (void **)&p));
    for (i = 0; i < COPY_BYTES / 4; i++)
        p[i] = i * 2654435761u;
    ID3D12Resource_Unmap(up, 0, NULL);

    HR_CHECK(ID3D12Device_CreateCommandAllocator(dev, D3D12_COMMAND_LIST_TYPE_COPY,
                                                 &IID_ID3D12CommandAllocator, (void **)&alloc));
    HR_CHECK(ID3D12Device_CreateCommandList(dev, 0, D3D12_COMMAND_LIST_TYPE_COPY, alloc, NULL,
                                            &IID_ID3D12GraphicsCommandList, (void **)&cl));
    transition(cl, mid, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_DEST);
    ID3D12GraphicsCommandList_CopyBufferRegion(cl, mid, 0, up, 0, COPY_BYTES);
    transition(cl, mid, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE);
    ID3D12GraphicsCommandList_CopyBufferRegion(cl, rb, 0, mid, 0, COPY_BYTES);
    transition(cl, mid, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COMMON);
    HR_CHECK(ID3D12GraphicsCommandList_Close(cl));
    execute(cq, cl);
    if (signal_wait(cq, "copy queue"))
        return 1;

    range.End = COPY_BYTES;
    HR_CHECK(ID3D12Resource_Map(rb, 0, &range, (void **)&p));
    for (i = 0; i < COPY_BYTES / 4; i++)
        if (p[i] != i * 2654435761u) {
            if (bad < 5)
                logmsg("mismatch at %u: got 0x%08x want 0x%08x", i, p[i], i * 2654435761u);
            bad++;
        }
    range.End = 0;
    ID3D12Resource_Unmap(rb, 0, &range);
    ID3D12GraphicsCommandList_Release(cl);
    ID3D12CommandAllocator_Release(alloc);
    ID3D12Resource_Release(rb);
    ID3D12Resource_Release(mid);
    ID3D12Resource_Release(up);
    if (bad)
        return fail("copy queue test: %u of %u words wrong", bad, COPY_BYTES / 4);
    logmsg("copy queue test: all %u words correct", COPY_BYTES / 4);
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
    PFN_SerializeRootSignature serialize;
    ID3DBlob *code = NULL, *errors = NULL, *rs_blob = NULL;
    HRESULT hr;
    D3D12_ROOT_PARAMETER param;
    D3D12_ROOT_SIGNATURE_DESC rsd;
    ID3D12RootSignature *rs;
    D3D12_COMPUTE_PIPELINE_STATE_DESC pd;
    ID3D12PipelineState *pso;
    ID3D12Resource *buf, *rb;
    D3D12_RANGE range;
    unsigned *p, i, bad = 0;

    logmsg("compute test: loading d3dcompiler_47.dll");
    lib = LoadLibraryA("d3dcompiler_47.dll");
    compile = (pD3DCompile)load_sym(lib, "D3DCompile");
    if (!compile) {
        logmsg("WARNING: d3dcompiler_47.dll / D3DCompile not available, skipping compute test");
        return 0;
    }
    serialize = (PFN_SerializeRootSignature)load_sym(d3d12_lib, "D3D12SerializeRootSignature");
    if (!serialize)
        return fail("d3d12.dll does not export D3D12SerializeRootSignature");
    hr = compile(cs_hlsl, sizeof(cs_hlsl) - 1, "cs.hlsl", NULL, NULL, "main", "cs_5_0",
                 D3DCOMPILE_OPTIMIZATION_LEVEL3, 0, &code, &errors);
    if (FAILED(hr))
        return fail("D3DCompile failed hr 0x%08lx: %s", (unsigned long)hr,
                    errors ? (const char *)ID3D10Blob_GetBufferPointer(errors) : "(no log)");
    if (errors)
        ID3D10Blob_Release(errors);
    errors = NULL;
    logmsg("compute test: cs_5_0 compiled, %u bytes DXBC", (unsigned)ID3D10Blob_GetBufferSize(code));

    memset(&param, 0, sizeof(param));
    param.ParameterType = D3D12_ROOT_PARAMETER_TYPE_UAV;
    param.Descriptor.ShaderRegister = 0;
    param.Descriptor.RegisterSpace = 0;
    param.ShaderVisibility = D3D12_SHADER_VISIBILITY_ALL;
    memset(&rsd, 0, sizeof(rsd));
    rsd.NumParameters = 1;
    rsd.pParameters = &param;
    rsd.Flags = D3D12_ROOT_SIGNATURE_FLAG_NONE;
    hr = serialize(&rsd, D3D_ROOT_SIGNATURE_VERSION_1, &rs_blob, &errors);
    if (FAILED(hr))
        return fail("D3D12SerializeRootSignature failed hr 0x%08lx: %s", (unsigned long)hr,
                    errors ? (const char *)ID3D10Blob_GetBufferPointer(errors) : "(no log)");
    if (errors)
        ID3D10Blob_Release(errors);
    HR_CHECK(ID3D12Device_CreateRootSignature(dev, 0, ID3D10Blob_GetBufferPointer(rs_blob),
                                              ID3D10Blob_GetBufferSize(rs_blob),
                                              &IID_ID3D12RootSignature, (void **)&rs));
    ID3D10Blob_Release(rs_blob);
    memset(&pd, 0, sizeof(pd));
    pd.pRootSignature = rs;
    pd.CS.pShaderBytecode = ID3D10Blob_GetBufferPointer(code);
    pd.CS.BytecodeLength = ID3D10Blob_GetBufferSize(code);
    HR_CHECK(ID3D12Device_CreateComputePipelineState(dev, &pd, &IID_ID3D12PipelineState, (void **)&pso));
    ID3D10Blob_Release(code);

    if (create_buffer(D3D12_HEAP_TYPE_DEFAULT, CS_COUNT * 4, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                      D3D12_RESOURCE_STATE_COMMON, &buf) ||
        create_buffer(D3D12_HEAP_TYPE_READBACK, CS_COUNT * 4, D3D12_RESOURCE_FLAG_NONE,
                      D3D12_RESOURCE_STATE_COPY_DEST, &rb))
        return 1;

    logmsg("compute test: Dispatch(%u, 1, 1) with a root UAV", CS_COUNT / 64);
    if (reset_direct_list(pso))
        return 1;
    transition(dcl, buf, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
    ID3D12GraphicsCommandList_SetComputeRootSignature(dcl, rs);
    ID3D12GraphicsCommandList_SetComputeRootUnorderedAccessView(dcl, 0,
                                                                ID3D12Resource_GetGPUVirtualAddress(buf));
    ID3D12GraphicsCommandList_Dispatch(dcl, CS_COUNT / 64, 1, 1);
    transition(dcl, buf, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE);
    ID3D12GraphicsCommandList_CopyBufferRegion(dcl, rb, 0, buf, 0, CS_COUNT * 4);
    transition(dcl, buf, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COMMON);
    HR_CHECK(ID3D12GraphicsCommandList_Close(dcl));
    execute(dq, dcl);
    if (signal_wait(dq, "dispatch+copy"))
        return 1;

    range.Begin = 0;
    range.End = CS_COUNT * 4;
    HR_CHECK(ID3D12Resource_Map(rb, 0, &range, (void **)&p));
    for (i = 0; i < CS_COUNT; i++)
        if (p[i] != i * 3u + 7u) {
            if (bad < 5)
                logmsg("mismatch at %u: got %u want %u", i, p[i], i * 3u + 7u);
            bad++;
        }
    range.End = 0;
    ID3D12Resource_Unmap(rb, 0, &range);
    ID3D12Resource_Release(rb);
    ID3D12Resource_Release(buf);
    ID3D12PipelineState_Release(pso);
    ID3D12RootSignature_Release(rs);
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

static int test_present(IDXGIFactory1 *factory1)
{
    WNDCLASSEXW wc;
    HWND hwnd;
    RECT rc = {0, 0, 640, 480};
    IDXGIFactory2 *factory2;
    DXGI_SWAP_CHAIN_DESC1 scd;
    IDXGISwapChain1 *sc1;
    IDXGISwapChain3 *sc;
    ID3D12Resource *bufs[SC_BUFFERS];
    ID3D12CommandAllocator *allocs[SC_BUFFERS];
    UINT64 frame_fence[SC_BUFFERS] = {0};
    D3D12_RESOURCE_DESC bd;
    HRESULT hr;
    int f;
    UINT i;
    double t0;

    logmsg("present test: creating window");
    memset(&wc, 0, sizeof(wc));
    wc.cbSize = sizeof(wc);
    wc.lpfnWndProc = wnd_proc;
    wc.hInstance = GetModuleHandleW(NULL);
    wc.hCursor = LoadCursorW(NULL, (LPCWSTR)IDC_ARROW);
    wc.lpszClassName = L"nvtrace_d3d12test";
    if (!RegisterClassExW(&wc))
        return fail("RegisterClassEx failed, error %lu", (unsigned long)GetLastError());
    AdjustWindowRect(&rc, WS_OVERLAPPEDWINDOW, FALSE);
    hwnd = CreateWindowExW(0, wc.lpszClassName, L"nvtrace d3d12test", WS_OVERLAPPEDWINDOW,
                           CW_USEDEFAULT, CW_USEDEFAULT, rc.right - rc.left, rc.bottom - rc.top,
                           NULL, NULL, wc.hInstance, NULL);
    if (!hwnd)
        return fail("CreateWindowEx failed, error %lu", (unsigned long)GetLastError());
    ShowWindow(hwnd, SW_SHOWNORMAL);
    pump_messages();

    HR_CHECK(IDXGIFactory1_QueryInterface(factory1, &IID_IDXGIFactory2, (void **)&factory2));
    memset(&scd, 0, sizeof(scd));
    scd.Width = 640;
    scd.Height = 480;
    scd.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    scd.SampleDesc.Count = 1;
    scd.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
    scd.BufferCount = SC_BUFFERS;
    scd.Scaling = DXGI_SCALING_STRETCH;
    scd.SwapEffect = DXGI_SWAP_EFFECT_FLIP_DISCARD;
    scd.AlphaMode = DXGI_ALPHA_MODE_UNSPECIFIED;
    logmsg("present test: CreateSwapChainForHwnd (640x480 R8G8B8A8, %u buffers, FLIP_DISCARD)", SC_BUFFERS);
    HR_CHECK(IDXGIFactory2_CreateSwapChainForHwnd(factory2, (IUnknown *)dq, hwnd, &scd, NULL, NULL, &sc1));
    HR_CHECK(IDXGISwapChain1_QueryInterface(sc1, &IID_IDXGISwapChain3, (void **)&sc));
    IDXGISwapChain1_Release(sc1);
    IDXGIFactory2_MakeWindowAssociation(factory2, hwnd, DXGI_MWA_NO_ALT_ENTER);

    for (i = 0; i < SC_BUFFERS; i++) {
        HR_CHECK(IDXGISwapChain3_GetBuffer(sc, i, &IID_ID3D12Resource, (void **)&bufs[i]));
        ID3D12Device_CreateRenderTargetView(dev, bufs[i], NULL, rtv_handle(1 + i));
        HR_CHECK(ID3D12Device_CreateCommandAllocator(dev, D3D12_COMMAND_LIST_TYPE_DIRECT,
                                                     &IID_ID3D12CommandAllocator, (void **)&allocs[i]));
    }
    bufs[0]->lpVtbl->GetDesc(bufs[0], &bd); /* struct return via hidden pointer */
    logmsg("present test: back buffer %llux%u format %d", (unsigned long long)bd.Width, bd.Height,
           (int)bd.Format);

    t0 = now_ms();
    for (f = 0; f < FRAMES; f++) {
        UINT idx = IDXGISwapChain3_GetCurrentBackBufferIndex(sc);
        float t = (float)f / (float)(FRAMES - 1);
        float color[4];
        color[0] = t;
        color[1] = 1.0f - t;
        color[2] = (f & 1) ? 0.5f : 0.25f;
        color[3] = 1.0f;
        pump_messages();
        if (idx >= SC_BUFFERS)
            return fail("GetCurrentBackBufferIndex returned %u", idx);
        if (wait_value(frame_fence[idx], "frame fence"))
            return 1;
        HR_CHECK(ID3D12CommandAllocator_Reset(allocs[idx]));
        HR_CHECK(ID3D12GraphicsCommandList_Reset(dcl, allocs[idx], NULL));
        transition(dcl, bufs[idx], D3D12_RESOURCE_STATE_PRESENT, D3D12_RESOURCE_STATE_RENDER_TARGET);
        ID3D12GraphicsCommandList_ClearRenderTargetView(dcl, rtv_handle(1 + idx), color, 0, NULL);
        transition(dcl, bufs[idx], D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_PRESENT);
        HR_CHECK(ID3D12GraphicsCommandList_Close(dcl));
        execute(dq, dcl);
        hr = IDXGISwapChain3_Present(sc, 1, 0);
        if (FAILED(hr))
            return fail("Present frame %d failed hr 0x%08lx (device removed reason 0x%08lx)", f,
                        (unsigned long)hr, (unsigned long)ID3D12Device_GetDeviceRemovedReason(dev));
        if (hr != S_OK && f == 0)
            logmsg("present test: Present returned status 0x%08lx", (unsigned long)hr);
        frame_fence[idx] = ++fence_val;
        HR_CHECK(ID3D12CommandQueue_Signal(dq, fence, frame_fence[idx]));
    }
    if (signal_wait(dq, "present drain"))
        return 1;
    logmsg("present test: %d frames in %.1f ms", FRAMES, now_ms() - t0);

    logmsg("present test: destroying swap chain and window");
    for (i = 0; i < SC_BUFFERS; i++) {
        ID3D12Resource_Release(bufs[i]);
        ID3D12CommandAllocator_Release(allocs[i]);
    }
    IDXGISwapChain3_Release(sc);
    IDXGIFactory2_Release(factory2);
    DestroyWindow(hwnd);
    pump_messages();
    UnregisterClassW(wc.lpszClassName, wc.hInstance);
    return 0;
}

int main(int argc, char **argv)
{
    HMODULE dxgi_lib;
    PFN_CreateDXGIFactory1 create_factory;
    PFN_D3D12_CREATE_DEVICE create_device;
    IDXGIFactory1 *factory;
    IDXGIAdapter1 *adapter, *chosen = NULL;
    DXGI_ADAPTER_DESC1 ad;
    D3D12_COMMAND_QUEUE_DESC qd;
    D3D12_DESCRIPTOR_HEAP_DESC hd;
    LUID luid;
    UINT i;
    char name[256];
    HRESULT hr;
    int no_present = has_arg(argc, argv, "--no-present");

    common_init("d3d12test");

    logmsg("loading dxgi.dll and d3d12.dll");
    dxgi_lib = LoadLibraryA("dxgi.dll");
    d3d12_lib = LoadLibraryA("d3d12.dll");
    create_factory = (PFN_CreateDXGIFactory1)load_sym(dxgi_lib, "CreateDXGIFactory1");
    create_device = (PFN_D3D12_CREATE_DEVICE)load_sym(d3d12_lib, "D3D12CreateDevice");
    if (!create_factory)
        return fail("dxgi.dll / CreateDXGIFactory1 not available");
    if (!create_device)
        return fail("d3d12.dll / D3D12CreateDevice not available");

    if (has_arg(argc, argv, "--debug")) {
        PFN_GetDebugInterface get_debug =
            (PFN_GetDebugInterface)load_sym(d3d12_lib, "D3D12GetDebugInterface");
        ID3D12Debug *dbg;
        if (get_debug && SUCCEEDED(get_debug(&IID_ID3D12Debug, (void **)&dbg))) {
            ID3D12Debug_EnableDebugLayer(dbg);
            ID3D12Debug_Release(dbg);
            logmsg("D3D12 debug layer enabled (messages go to the debugger output)");
        } else {
            logmsg("WARNING: --debug: D3D12 debug layer not available (install Graphics Tools)");
        }
    }

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

    logmsg("D3D12CreateDevice (feature level 11_0)");
    hr = create_device((IUnknown *)chosen, D3D_FEATURE_LEVEL_11_0, &IID_ID3D12Device, (void **)&dev);
    if (FAILED(hr))
        return fail("D3D12CreateDevice failed: hr 0x%08lx", (unsigned long)hr);
    dev->lpVtbl->GetAdapterLuid(dev, &luid); /* struct return via hidden pointer */
    logmsg("device created, device LUID %08lx:%08lx%s", (unsigned long)luid.HighPart,
           (unsigned long)luid.LowPart,
           (luid.HighPart == ad.AdapterLuid.HighPart && luid.LowPart == ad.AdapterLuid.LowPart)
               ? " (matches adapter)" : " (DOES NOT match adapter)");

    logmsg("creating direct + copy queues, fence, RTV heap, command list");
    memset(&qd, 0, sizeof(qd));
    qd.Type = D3D12_COMMAND_LIST_TYPE_DIRECT;
    HR_CHECK(ID3D12Device_CreateCommandQueue(dev, &qd, &IID_ID3D12CommandQueue, (void **)&dq));
    qd.Type = D3D12_COMMAND_LIST_TYPE_COPY;
    HR_CHECK(ID3D12Device_CreateCommandQueue(dev, &qd, &IID_ID3D12CommandQueue, (void **)&cq));
    HR_CHECK(ID3D12Device_CreateFence(dev, 0, D3D12_FENCE_FLAG_NONE, &IID_ID3D12Fence, (void **)&fence));
    fence_event = CreateEventW(NULL, FALSE, FALSE, NULL);
    if (!fence_event)
        return fail("CreateEvent failed, error %lu", (unsigned long)GetLastError());
    memset(&hd, 0, sizeof(hd));
    hd.Type = D3D12_DESCRIPTOR_HEAP_TYPE_RTV;
    hd.NumDescriptors = 1 + SC_BUFFERS;
    HR_CHECK(ID3D12Device_CreateDescriptorHeap(dev, &hd, &IID_ID3D12DescriptorHeap, (void **)&rtv_heap));
    rtv_inc = ID3D12Device_GetDescriptorHandleIncrementSize(dev, D3D12_DESCRIPTOR_HEAP_TYPE_RTV);
    logmsg("RTV heap start 0x%llx, increment %u", (unsigned long long)rtv_handle(0).ptr, rtv_inc);
    HR_CHECK(ID3D12Device_CreateCommandAllocator(dev, D3D12_COMMAND_LIST_TYPE_DIRECT,
                                                 &IID_ID3D12CommandAllocator, (void **)&dalloc));
    HR_CHECK(ID3D12Device_CreateCommandList(dev, 0, D3D12_COMMAND_LIST_TYPE_DIRECT, dalloc, NULL,
                                            &IID_ID3D12GraphicsCommandList, (void **)&dcl));
    HR_CHECK(ID3D12GraphicsCommandList_Close(dcl)); /* reset_direct_list() reopens it */

    if (test_clear())
        return 1;
    if (test_copy_queue())
        return 1;
    if (test_compute())
        return 1;
    if (no_present)
        logmsg("--no-present: skipping window/swap chain test");
    else if (test_present(factory))
        return 1;

    hr = ID3D12Device_GetDeviceRemovedReason(dev);
    if (hr != S_OK)
        return fail("device removed, reason 0x%08lx", (unsigned long)hr);
    logmsg("cleanup");
    ID3D12GraphicsCommandList_Release(dcl);
    ID3D12CommandAllocator_Release(dalloc);
    ID3D12DescriptorHeap_Release(rtv_heap);
    ID3D12Fence_Release(fence);
    CloseHandle(fence_event);
    ID3D12CommandQueue_Release(cq);
    ID3D12CommandQueue_Release(dq);
    ID3D12Device_Release(dev);
    IDXGIAdapter1_Release(chosen);
    IDXGIFactory1_Release(factory);
    return pass();
}
