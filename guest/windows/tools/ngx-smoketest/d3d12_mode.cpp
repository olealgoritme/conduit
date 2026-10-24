// ngx-smoketest, D3D12 mode: the same run through NGX's D3D12 path (which on
// our stack goes through nvapi64 and the D3D12 driver's engine-object
// meta-commands, see guest/nvk-rm/docs/dlss.md).
#include "smoketest.h"

#include <d3d12.h>
#include <dxgi1_4.h>

#include "nvsdk_ngx.h"
#include "nvsdk_ngx_helpers_d3d.h"


static IDXGIFactory4 *factory;
static IDXGIAdapter1 *adapter;
static ID3D12Device *device;
static ID3D12CommandQueue *queue;
static ID3D12CommandAllocator *alloc;
static ID3D12GraphicsCommandList *list;
static ID3D12Fence *fence;
static HANDLE fence_event;
static UINT64 fence_value;
static ID3D12Resource *color, *depth, *mvec, *output, *upload, *readback, *tsbuf;
static ID3D12QueryHeap *qheap;
static bool ngx_inited;
static NVSDK_NGX_Parameter *caps;
static NVSDK_NGX_Handle *feature;

template <class T> static void release(T *&p)
{
    if (p) {
        p->Release();
        p = NULL;
    }
}

static bool hrcheck(const char *what, HRESULT hr)
{
    if (SUCCEEDED(hr))
        return true;
    fail("%s -> HRESULT 0x%08lx", what, (unsigned long)hr);
    if (device) {
        HRESULT why = device->GetDeviceRemovedReason();
        if (why != S_OK)
            hang("device removed (reason 0x%08lx); stopping", (unsigned long)why);
    }
    return false;
}

static void begin_list()
{
    alloc->Reset();
    list->Reset(alloc, NULL);
}

static bool submit_wait(const char *what, double *cpu_ms)
{
    if (!hrcheck("ID3D12GraphicsCommandList::Close", list->Close()))
        return false;
    ID3D12CommandList *lists[] = {list};
    double t0 = now_ms();
    queue->ExecuteCommandLists(1, lists);
    if (!hrcheck("ID3D12CommandQueue::Signal", queue->Signal(fence, ++fence_value)))
        return false;
    if (fence->GetCompletedValue() < fence_value) {
        if (!hrcheck("ID3D12Fence::SetEventOnCompletion", fence->SetEventOnCompletion(fence_value, fence_event)))
            return false;
        if (WaitForSingleObject(fence_event, g_opt.timeout_ms) == WAIT_TIMEOUT)
            hang("%s: fence not reached after %lu ms (GPU hang?)", what, (unsigned long)g_opt.timeout_ms);
    }
    if (cpu_ms)
        *cpu_ms = now_ms() - t0;
    HRESULT why = device->GetDeviceRemovedReason();
    if (why != S_OK)
        hang("%s: device removed (reason 0x%08lx)", what, (unsigned long)why);
    return true;
}

static void barrier(ID3D12Resource *r, D3D12_RESOURCE_STATES from, D3D12_RESOURCE_STATES to)
{
    D3D12_RESOURCE_BARRIER b = {};
    b.Type = D3D12_RESOURCE_BARRIER_TYPE_TRANSITION;
    b.Transition.pResource = r;
    b.Transition.Subresource = D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES;
    b.Transition.StateBefore = from;
    b.Transition.StateAfter = to;
    list->ResourceBarrier(1, &b);
}

static D3D12_RESOURCE_DESC tex_desc(DXGI_FORMAT fmt, unsigned w, unsigned h, D3D12_RESOURCE_FLAGS flags)
{
    D3D12_RESOURCE_DESC d = {};
    d.Dimension = D3D12_RESOURCE_DIMENSION_TEXTURE2D;
    d.Width = w;
    d.Height = h;
    d.DepthOrArraySize = 1;
    d.MipLevels = 1;
    d.Format = fmt;
    d.SampleDesc.Count = 1;
    d.Layout = D3D12_TEXTURE_LAYOUT_UNKNOWN;
    d.Flags = flags;
    return d;
}

static bool uav_ok(DXGI_FORMAT fmt)
{
    D3D12_FEATURE_DATA_FORMAT_SUPPORT fs = {fmt, D3D12_FORMAT_SUPPORT1_NONE, D3D12_FORMAT_SUPPORT2_NONE};
    if (FAILED(device->CheckFeatureSupport(D3D12_FEATURE_FORMAT_SUPPORT, &fs, sizeof(fs))))
        return false;
    return (fs.Support1 & D3D12_FORMAT_SUPPORT1_TYPED_UNORDERED_ACCESS_VIEW) != 0;
}

static bool create_tex(ID3D12Resource **r, const char *name, DXGI_FORMAT fmt, unsigned w, unsigned h, bool need_uav)
{
    D3D12_RESOURCE_FLAGS flags = D3D12_RESOURCE_FLAG_NONE;
    if (need_uav || uav_ok(fmt))
        flags = D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS;
    D3D12_RESOURCE_DESC d = tex_desc(fmt, w, h, flags);
    D3D12_HEAP_PROPERTIES hp = {};
    hp.Type = D3D12_HEAP_TYPE_DEFAULT;
    char what[96];
    snprintf(what, sizeof(what), "CreateCommittedResource(%s %ux%u format %d)", name, w, h, (int)fmt);
    return hrcheck(what, device->CreateCommittedResource(&hp, D3D12_HEAP_FLAG_NONE, &d, D3D12_RESOURCE_STATE_COPY_DEST,
                                                         NULL, IID_PPV_ARGS(r)));
}

static bool create_buffer(ID3D12Resource **r, UINT64 size, D3D12_HEAP_TYPE type)
{
    D3D12_RESOURCE_DESC d = {};
    d.Dimension = D3D12_RESOURCE_DIMENSION_BUFFER;
    d.Width = size;
    d.Height = 1;
    d.DepthOrArraySize = 1;
    d.MipLevels = 1;
    d.Format = DXGI_FORMAT_UNKNOWN;
    d.SampleDesc.Count = 1;
    d.Layout = D3D12_TEXTURE_LAYOUT_ROW_MAJOR;
    D3D12_HEAP_PROPERTIES hp = {};
    hp.Type = type;
    D3D12_RESOURCE_STATES st =
        type == D3D12_HEAP_TYPE_UPLOAD ? D3D12_RESOURCE_STATE_GENERIC_READ : D3D12_RESOURCE_STATE_COPY_DEST;
    return hrcheck("CreateCommittedResource(buffer)",
                   device->CreateCommittedResource(&hp, D3D12_HEAP_FLAG_NONE, &d, st, NULL, IID_PPV_ARGS(r)));
}

struct Footprint {
    D3D12_PLACED_SUBRESOURCE_FOOTPRINT fp;
    UINT rows;
    UINT64 row_bytes, total;
};

static Footprint footprint(DXGI_FORMAT fmt, unsigned w, unsigned h, UINT64 offset)
{
    Footprint f = {};
    D3D12_RESOURCE_DESC d = tex_desc(fmt, w, h, D3D12_RESOURCE_FLAG_NONE);
    device->GetCopyableFootprints(&d, 0, 1, offset, &f.fp, &f.rows, &f.row_bytes, &f.total);
    return f;
}

static void copy_to_tex(ID3D12Resource *tex, const Footprint &f)
{
    D3D12_TEXTURE_COPY_LOCATION dst = {}, src = {};
    dst.pResource = tex;
    dst.Type = D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX;
    dst.SubresourceIndex = 0;
    src.pResource = upload;
    src.Type = D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT;
    src.PlacedFootprint = f.fp;
    list->CopyTextureRegion(&dst, 0, 0, 0, &src, NULL);
}

static void cleanup()
{
    step("shutdown");
    if (feature)
        ngx_check("NVSDK_NGX_D3D12_ReleaseFeature", NVSDK_NGX_D3D12_ReleaseFeature(feature));
    if (caps)
        ngx_check("NVSDK_NGX_D3D12_DestroyParameters", NVSDK_NGX_D3D12_DestroyParameters(caps));
    if (ngx_inited)
        ngx_check("NVSDK_NGX_D3D12_Shutdown1", NVSDK_NGX_D3D12_Shutdown1(device));
    feature = NULL;
    caps = NULL;
    ngx_inited = false;
    release(qheap);
    release(tsbuf);
    release(readback);
    release(upload);
    release(output);
    release(mvec);
    release(depth);
    release(color);
    release(fence);
    release(list);
    release(alloc);
    release(queue);
    release(device);
    release(adapter);
    release(factory);
    if (fence_event)
        CloseHandle(fence_event);
    fence_event = NULL;
    ok("D3D12 objects released");
}

static int finish()
{
    cleanup();
    return failures() ? 1 : 0;
}

int run_d3d12()
{
    NVSDK_NGX_FeatureCommonInfo ci;
    NVSDK_NGX_FeatureDiscoveryInfo di;
    ngx_common_info(&ci);
    ngx_discovery_info(&di, &ci);

    step("DXGI adapters");
    if (!hrcheck("CreateDXGIFactory1", CreateDXGIFactory1(IID_PPV_ARGS(&factory))))
        return 1;
    int pick = -1;
    for (UINT i = 0; i < 16; i++) {
        IDXGIAdapter1 *a = NULL;
        if (factory->EnumAdapters1(i, &a) == DXGI_ERROR_NOT_FOUND)
            break;
        DXGI_ADAPTER_DESC1 d;
        a->GetDesc1(&d);
        out("       [%u] %ls  vendor 0x%04x device 0x%04x subsys 0x%08x  %llu MB  LUID %08lx:%08lx%s", i,
            d.Description, d.VendorId, d.DeviceId, d.SubSysId,
            (unsigned long long)(d.DedicatedVideoMemory >> 20), (unsigned long)d.AdapterLuid.HighPart,
            (unsigned long)d.AdapterLuid.LowPart, (d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE) ? "  (software)" : "");
        bool want = g_opt.device >= 0 ? (int)i == g_opt.device
                                      : pick < 0 && !(d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE);
        if (want && pick < 0) {
            pick = (int)i;
            adapter = a;
        } else
            a->Release();
    }
    if (!adapter) {
        fail("no usable adapter");
        return finish();
    }
    ok("using adapter [%d]", pick);

    step("NGX feature requirements");
    NVSDK_NGX_FeatureRequirement req;
    memset(&req, 0, sizeof(req));
    if (ngx_check("NVSDK_NGX_D3D12_GetFeatureRequirements",
                  NVSDK_NGX_D3D12_GetFeatureRequirements(adapter, &di, &req))) {
        ngx_print_requirement(&req);
        if (req.FeatureSupported != NVSDK_NGX_FeatureSupportResult_Supported)
            fail("NGX reports SuperSampling as not supported on this adapter");
    }

    step("D3D12CreateDevice");
    if (!hrcheck("D3D12CreateDevice(FL 11_0)",
                 D3D12CreateDevice(adapter, D3D_FEATURE_LEVEL_11_0, IID_PPV_ARGS(&device))))
        return finish();
    D3D12_COMMAND_QUEUE_DESC qd = {};
    qd.Type = D3D12_COMMAND_LIST_TYPE_DIRECT;
    if (!hrcheck("CreateCommandQueue", device->CreateCommandQueue(&qd, IID_PPV_ARGS(&queue))) ||
        !hrcheck("CreateCommandAllocator",
                 device->CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT, IID_PPV_ARGS(&alloc))) ||
        !hrcheck("CreateCommandList", device->CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, alloc, NULL,
                                                                IID_PPV_ARGS(&list))) ||
        !hrcheck("CreateFence", device->CreateFence(0, D3D12_FENCE_FLAG_NONE, IID_PPV_ARGS(&fence))))
        return finish();
    list->Close();
    fence_event = CreateEventW(NULL, FALSE, FALSE, NULL);
    UINT64 ts_freq = 0;
    if (FAILED(queue->GetTimestampFrequency(&ts_freq)))
        ts_freq = 0;
    ok("device, direct queue (timestamp frequency %llu Hz)", (unsigned long long)ts_freq);

    step("NGX init");
    double t0 = now_ms();
    NVSDK_NGX_Result r;
    if (g_opt.use_app_id)
        r = NVSDK_NGX_D3D12_Init(g_opt.app_id, g_opt.logdir, device, &ci, NVSDK_NGX_Version_API);
    else
        r = NVSDK_NGX_D3D12_Init_with_ProjectID(g_opt.project_id, NVSDK_NGX_ENGINE_TYPE_CUSTOM, "1.0", g_opt.logdir,
                                                device, &ci, NVSDK_NGX_Version_API);
    out("       took %.1f ms", now_ms() - t0);
    ngx_inited = ngx_check(g_opt.use_app_id ? "NVSDK_NGX_D3D12_Init" : "NVSDK_NGX_D3D12_Init_with_ProjectID", r);
    report_loaded_modules();
    if (!ngx_inited)
        return finish();

    step("NGX capability parameters");
    if (!ngx_check("NVSDK_NGX_D3D12_GetCapabilityParameters", NVSDK_NGX_D3D12_GetCapabilityParameters(&caps)))
        return finish();
    if (!ngx_print_caps(caps)) {
        fail("SuperSampling.Available is not 1: DLSS is not available");
        return finish();
    }
    ok("SuperSampling available");

    step("render size");
    unsigned rw, rh, ow = g_opt.out_w, oh = g_opt.out_h;
    ngx_render_size(caps, &rw, &rh);

    step("scratch buffer size");
    size_t scratch = 0;
    r = NVSDK_NGX_D3D12_GetScratchBufferSize(NVSDK_NGX_Feature_SuperSampling, caps, &scratch);
    if (NVSDK_NGX_SUCCEED(r))
        ok("NVSDK_NGX_D3D12_GetScratchBufferSize -> 0x%08x %s: %zu bytes", (unsigned)r, ngx_result_name(r), scratch);
    else
        warn("NVSDK_NGX_D3D12_GetScratchBufferSize -> 0x%08x %s", (unsigned)r, ngx_result_name(r));

    step("resources");
    if (!create_tex(&color, "color", DXGI_FORMAT_R8G8B8A8_UNORM, rw, rh, false) ||
        !create_tex(&depth, "depth", DXGI_FORMAT_R32_FLOAT, rw, rh, false) ||
        !create_tex(&mvec, "motion vectors", DXGI_FORMAT_R16G16_FLOAT, rw, rh, false) ||
        !create_tex(&output, "output", DXGI_FORMAT_R8G8B8A8_UNORM, ow, oh, true))
        return finish();
    const UINT64 align = D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT;
    Footprint fc = footprint(DXGI_FORMAT_R8G8B8A8_UNORM, rw, rh, 0);
    Footprint fd = footprint(DXGI_FORMAT_R32_FLOAT, rw, rh, (fc.total + align - 1) & ~(align - 1));
    Footprint fm = footprint(DXGI_FORMAT_R16G16_FLOAT, rw, rh, (fd.fp.Offset + fd.total + align - 1) & ~(align - 1));
    Footprint fo = footprint(DXGI_FORMAT_R8G8B8A8_UNORM, ow, oh, (fm.fp.Offset + fm.total + align - 1) & ~(align - 1));
    Footprint frb = footprint(DXGI_FORMAT_R8G8B8A8_UNORM, ow, oh, 0);
    if (!create_buffer(&upload, fo.fp.Offset + fo.total, D3D12_HEAP_TYPE_UPLOAD) ||
        !create_buffer(&readback, frb.total, D3D12_HEAP_TYPE_READBACK) ||
        !create_buffer(&tsbuf, 2 * sizeof(UINT64), D3D12_HEAP_TYPE_READBACK))
        return finish();
    D3D12_QUERY_HEAP_DESC qhd = {};
    qhd.Type = D3D12_QUERY_HEAP_TYPE_TIMESTAMP;
    qhd.Count = 2;
    bool timestamps = ts_freq && hrcheck("CreateQueryHeap", device->CreateQueryHeap(&qhd, IID_PPV_ARGS(&qheap)));
    if (!ts_freq)
        warn("no timestamp frequency; GPU times are not measured");

    uint8_t *host_color = (uint8_t *)malloc((size_t)rw * rh * 4);
    make_color(host_color, rw, rh, (size_t)rw * 4);
    uint8_t *up = NULL;
    D3D12_RANGE none = {0, 0};
    if (!hrcheck("Map(upload)", upload->Map(0, &none, (void **)&up)))
        return finish();
    for (unsigned y = 0; y < rh; y++)
        memcpy(up + fc.fp.Offset + (size_t)y * fc.fp.Footprint.RowPitch, host_color + (size_t)y * rw * 4,
               (size_t)rw * 4);
    make_depth((float *)(up + fd.fp.Offset), rw, rh, fd.fp.Footprint.RowPitch);
    for (unsigned y = 0; y < rh; y++)
        memset(up + fm.fp.Offset + (size_t)y * fm.fp.Footprint.RowPitch, 0, (size_t)rw * 4);
    for (unsigned y = 0; y < oh; y++) {
        uint8_t *row = up + fo.fp.Offset + (size_t)y * fo.fp.Footprint.RowPitch;
        for (unsigned x = 0; x < ow; x++)
            memcpy(row + x * 4, kClear, 4);
    }
    upload->Unmap(0, NULL);
    begin_list();
    copy_to_tex(color, fc);
    copy_to_tex(depth, fd);
    copy_to_tex(mvec, fm);
    copy_to_tex(output, fo);
    const D3D12_RESOURCE_STATES srv =
        D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE | D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE;
    barrier(color, D3D12_RESOURCE_STATE_COPY_DEST, srv);
    barrier(depth, D3D12_RESOURCE_STATE_COPY_DEST, srv);
    barrier(mvec, D3D12_RESOURCE_STATE_COPY_DEST, srv);
    barrier(output, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
    if (!submit_wait("upload", NULL))
        return finish();
    ok("color %ux%u RGBA8, depth R32F, motion vectors RG16F (zero), output %ux%u RGBA8 (filled with 255,0,255)", rw,
       rh, ow, oh);

    step("create DLSS feature");
    ngx_set_create_hints(caps);
    NVSDK_NGX_DLSS_Create_Params cp;
    memset(&cp, 0, sizeof(cp));
    cp.Feature.InWidth = rw;
    cp.Feature.InHeight = rh;
    cp.Feature.InTargetWidth = ow;
    cp.Feature.InTargetHeight = oh;
    cp.Feature.InPerfQualityValue = g_opt.quality;
    cp.InFeatureCreateFlags = ngx_create_flags();
    begin_list();
    t0 = now_ms();
    r = NGX_D3D12_CREATE_DLSS_EXT(list, 1, 1, &feature, caps, &cp);
    double t_create = now_ms() - t0;
    if (!ngx_check("NGX_D3D12_CREATE_DLSS_EXT (NVSDK_NGX_D3D12_CreateFeature)", r)) {
        list->Close();
        feature = NULL;
        return finish();
    }
    double t_submit = 0;
    if (!submit_wait("feature creation", &t_submit))
        return finish();
    ok("feature created (CPU %.1f ms, its command list %.1f ms)", t_create, t_submit);

    step("evaluate");
    Timings tm;
    timings_alloc(&tm, g_opt.frames);
    int done = 0;
    for (int f = 0; f < g_opt.frames; f++) {
        begin_list();
        if (timestamps)
            list->EndQuery(qheap, D3D12_QUERY_TYPE_TIMESTAMP, 0);
        NVSDK_NGX_D3D12_DLSS_Eval_Params ep;
        memset(&ep, 0, sizeof(ep));
        ep.Feature.pInColor = color;
        ep.Feature.pInOutput = output;
        ep.pInDepth = depth;
        ep.pInMotionVectors = mvec;
        jitter(f, &ep.InJitterOffsetX, &ep.InJitterOffsetY);
        ep.InRenderSubrectDimensions.Width = rw;
        ep.InRenderSubrectDimensions.Height = rh;
        ep.InReset = f == 0;
        ep.InMVScaleX = 1.0f;
        ep.InMVScaleY = 1.0f;
        double c0 = now_ms();
        r = NGX_D3D12_EVALUATE_DLSS_EXT(list, feature, caps, &ep);
        tm.cpu_eval[f] = now_ms() - c0;
        if (NVSDK_NGX_FAILED(r)) {
            fail("frame %d: NGX_D3D12_EVALUATE_DLSS_EXT -> 0x%08x %s", f, (unsigned)r, ngx_result_name(r));
            list->Close();
            break;
        }
        if (timestamps) {
            list->EndQuery(qheap, D3D12_QUERY_TYPE_TIMESTAMP, 1);
            list->ResolveQueryData(qheap, D3D12_QUERY_TYPE_TIMESTAMP, 0, 2, tsbuf, 0);
        }
        char what[48];
        snprintf(what, sizeof(what), "evaluate frame %d", f);
        if (!submit_wait(what, &tm.cpu_frame[f]))
            break;
        tm.gpu[f] = -1;
        if (timestamps) {
            UINT64 *ts = NULL;
            D3D12_RANGE rr = {0, 2 * sizeof(UINT64)};
            if (SUCCEEDED(tsbuf->Map(0, &rr, (void **)&ts)) && ts) {
                tm.gpu[f] = (double)(ts[1] - ts[0]) * 1000.0 / (double)ts_freq;
                tsbuf->Unmap(0, &none);
            }
        }
        if (f < 3 || f == g_opt.frames - 1)
            out("       frame %3d: GPU %.3f ms, EvaluateFeature call %.3f ms, submit to signal %.3f ms", f, tm.gpu[f],
                tm.cpu_eval[f], tm.cpu_frame[f]);
        done++;
    }
    if (done == g_opt.frames)
        ok("%d frames evaluated (NGX result 0x%08x %s on the last)", done, (unsigned)r, ngx_result_name(r));
    else
        fail("only %d of %d frames evaluated", done, g_opt.frames);
    tm.n = done;
    timings_print(&tm);

    step("readback");
    begin_list();
    barrier(output, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE);
    D3D12_TEXTURE_COPY_LOCATION dst = {}, src = {};
    dst.pResource = readback;
    dst.Type = D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT;
    dst.PlacedFootprint = frb.fp;
    src.pResource = output;
    src.Type = D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX;
    src.SubresourceIndex = 0;
    list->CopyTextureRegion(&dst, 0, 0, 0, &src, NULL);
    if (submit_wait("readback", NULL)) {
        uint8_t *px = NULL;
        D3D12_RANGE rr = {0, (SIZE_T)frb.total};
        if (hrcheck("Map(readback)", readback->Map(0, &rr, (void **)&px)) && px) {
            check_output(px + frb.fp.Offset, ow, oh, frb.fp.Footprint.RowPitch, host_color, rw, rh, (size_t)rw * 4);
            readback->Unmap(0, &none);
        }
    }
    free(host_color);
    return finish();
}
