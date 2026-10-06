// d3d12_tri: a small D3D12 check for dxvk-on-nvk S5 (D3D12 on NVK): caps,
// a compute dispatch, an offscreen clear + triangle read back and checked, then
// a spinning triangle presented through a flip-model swap chain, fps per second.
//
//   d3d12_tri.exe [seconds=5] [width=1280 height=720] [sync=0] [--offscreen]
//
// Exit code 0 only if the compute and offscreen pixel checks pass and every
// present succeeded. --offscreen skips the swap chain (works from an ssh
// session; a swap chain needs the interactive desktop, see run-in-session.ps1).
//
// d3d12.dll and dxgi.dll are whatever the loader resolves: the system runtime
// (the Helios UMD12 on the Helios adapter) or app-local vkd3d-proton
// d3d12.dll/d3d12core.dll + DXVK dxgi.dll (standalone vkd3d-proton on NVK
// through a vulkan-1.dll shim). HELIOS_ICD=nvk|venus picks the UMD backend.
//
// Build (MinGW-w64 on Linux):
//   x86_64-w64-mingw32-g++ -O2 -static d3d12_tri.cpp -o d3d12_tri.exe
//       -ldxgi -ld3dcompiler -lgdi32 -luser32 -luuid
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d12.h>
#include <d3dcompiler.h>
#include <dxgi1_4.h>

#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <cstring>

#define CHECK(expr)                                                                   \
  do {                                                                                \
    HRESULT hr_ = (expr);                                                             \
    if (FAILED(hr_)) {                                                                \
      std::printf("FAIL %s:%d %s -> 0x%08lx\n", __FILE__, __LINE__, #expr, (unsigned long)hr_); \
      std::fflush(stdout);                                                            \
      std::exit(2);                                                                   \
    }                                                                                 \
  } while (0)

static const char kGfx[] = R"(
cbuffer C : register(b0) { float4 rot; };
struct V { float4 pos : SV_Position; float4 col : COLOR; };
V vs(uint id : SV_VertexID) {
  float2 p = id == 0 ? float2(0, 0.8) : (id == 1 ? float2(0.7, -0.6) : float2(-0.7, -0.6));
  float2 r = float2(p.x * rot.x - p.y * rot.y, p.x * rot.y + p.y * rot.x);
  V o;
  o.pos = float4(r.x * rot.z, r.y, 0.5, 1);
  o.col = id == 0 ? float4(1, 0, 0, 1) : (id == 1 ? float4(0, 1, 0, 1) : float4(0, 0, 1, 1));
  if (rot.w > 0.5) o.col = float4(1, 1, 0, 1); // flat yellow for the pixel check
  return o;
}
float4 ps(V i) : SV_Target { return i.col; }
)";

static const char kCompute[] = R"(
RWStructuredBuffer<uint> buf : register(u0);
[numthreads(64, 1, 1)]
void cs(uint3 id : SV_DispatchThreadID) { buf[id.x] = id.x * 3u + 1u; }
)";

typedef HRESULT(WINAPI* PfnCreateDevice)(IUnknown*, D3D_FEATURE_LEVEL, REFIID, void**);
typedef HRESULT(WINAPI* PfnSerializeRs)(const D3D12_ROOT_SIGNATURE_DESC*, D3D_ROOT_SIGNATURE_VERSION,
                                        ID3DBlob**, ID3DBlob**);

static ID3DBlob* compile(const char* src, size_t len, const char* entry, const char* target) {
  ID3DBlob* code = nullptr;
  ID3DBlob* err = nullptr;
  if (FAILED(D3DCompile(src, len, entry, nullptr, nullptr, entry, target, 0, 0, &code, &err))) {
    std::printf("D3DCompile %s: %s\n", entry, err ? (const char*)err->GetBufferPointer() : "?");
    std::exit(2);
  }
  return code;
}

static LRESULT CALLBACK wndproc(HWND h, UINT m, WPARAM w, LPARAM l) {
  if (m == WM_DESTROY) { PostQuitMessage(0); return 0; }
  return DefWindowProcA(h, m, w, l);
}

struct Gpu {
  ID3D12Device* dev = nullptr;
  ID3D12CommandQueue* queue = nullptr;
  ID3D12CommandAllocator* alloc = nullptr;
  ID3D12GraphicsCommandList* list = nullptr;
  ID3D12Fence* fence = nullptr;
  UINT64 value = 0;
  HANDLE event = nullptr;

  void submit_and_wait(const char* what) {
    CHECK(list->Close());
    ID3D12CommandList* lists[] = { list };
    queue->ExecuteCommandLists(1, lists);
    CHECK(queue->Signal(fence, ++value));
    if (fence->GetCompletedValue() < value) {
      CHECK(fence->SetEventOnCompletion(value, event));
      if (WaitForSingleObject(event, 5000) != WAIT_OBJECT_0) {
        std::printf("FAIL %s: fence %llu not reached in 5 s (completed %llu)\n", what,
                    (unsigned long long)value, (unsigned long long)fence->GetCompletedValue());
        std::exit(3);
      }
    }
    CHECK(alloc->Reset());
    CHECK(list->Reset(alloc, nullptr));
  }
};

static D3D12_RESOURCE_BARRIER transition(ID3D12Resource* r, D3D12_RESOURCE_STATES a, D3D12_RESOURCE_STATES b) {
  D3D12_RESOURCE_BARRIER t = {};
  t.Type = D3D12_RESOURCE_BARRIER_TYPE_TRANSITION;
  t.Transition.pResource = r;
  t.Transition.Subresource = D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES;
  t.Transition.StateBefore = a;
  t.Transition.StateAfter = b;
  return t;
}

static ID3D12Resource* buffer(ID3D12Device* dev, UINT64 size, D3D12_HEAP_TYPE type,
                              D3D12_RESOURCE_FLAGS flags, D3D12_RESOURCE_STATES state) {
  D3D12_HEAP_PROPERTIES hp = {};
  hp.Type = type;
  D3D12_RESOURCE_DESC rd = {};
  rd.Dimension = D3D12_RESOURCE_DIMENSION_BUFFER;
  rd.Width = size;
  rd.Height = 1;
  rd.DepthOrArraySize = 1;
  rd.MipLevels = 1;
  rd.SampleDesc.Count = 1;
  rd.Layout = D3D12_TEXTURE_LAYOUT_ROW_MAJOR;
  rd.Flags = flags;
  ID3D12Resource* r = nullptr;
  CHECK(dev->CreateCommittedResource(&hp, D3D12_HEAP_FLAG_NONE, &rd, state, nullptr,
                                     __uuidof(ID3D12Resource), (void**)&r));
  return r;
}

static const char* tier_name(int t) {
  static char buf[8][16];
  static int n = 0;
  char* b = buf[n++ & 7];
  std::snprintf(b, 16, "%d", t);
  return b;
}

int main(int argc, char** argv) {
  bool offscreen = false;
  int pos[4] = { 5, 1280, 720, 0 };
  int npos = 0;
  for (int i = 1; i < argc; i++) {
    if (!std::strcmp(argv[i], "--offscreen")) offscreen = true;
    else if (npos < 4) pos[npos++] = std::atoi(argv[i]);
  }
  const int seconds = pos[0];
  const UINT width = pos[1], height = pos[2];
  const UINT sync = pos[3];
  setvbuf(stdout, nullptr, _IONBF, 0);

  HMODULE d3d12 = LoadLibraryA("d3d12.dll");
  if (!d3d12) { std::printf("FAIL: d3d12.dll did not load (%lu)\n", GetLastError()); return 2; }
  char d3d12_path[MAX_PATH] = {};
  GetModuleFileNameA(d3d12, d3d12_path, MAX_PATH);
  auto create_device = (PfnCreateDevice)(void*)GetProcAddress(d3d12, "D3D12CreateDevice");
  auto serialize_rs = (PfnSerializeRs)(void*)GetProcAddress(d3d12, "D3D12SerializeRootSignature");
  if (!create_device || !serialize_rs) { std::printf("FAIL: d3d12.dll lacks exports\n"); return 2; }

  IDXGIFactory4* factory = nullptr;
  CHECK(CreateDXGIFactory2(0, __uuidof(IDXGIFactory4), (void**)&factory));
  HMODULE dxgi = GetModuleHandleA("dxgi.dll");
  char dxgi_path[MAX_PATH] = {};
  GetModuleFileNameA(dxgi, dxgi_path, MAX_PATH);
  std::printf("d3d12: %s\ndxgi:  %s\n", d3d12_path, dxgi_path);

  IDXGIAdapter1* adapter = nullptr;
  CHECK(factory->EnumAdapters1(0, &adapter));
  DXGI_ADAPTER_DESC1 ad = {};
  adapter->GetDesc1(&ad);
  std::printf("adapter: %ls vendor=0x%04x device=0x%04x luid=%08lx:%08lx vram=%llu MiB\n",
              ad.Description, ad.VendorId, ad.DeviceId, (unsigned long)ad.AdapterLuid.HighPart,
              (unsigned long)ad.AdapterLuid.LowPart, (unsigned long long)(ad.DedicatedVideoMemory >> 20));

  Gpu g;
  LARGE_INTEGER f0, f1, freq;
  QueryPerformanceFrequency(&freq);
  QueryPerformanceCounter(&f0);
  CHECK(create_device(adapter, D3D_FEATURE_LEVEL_11_0, __uuidof(ID3D12Device), (void**)&g.dev));
  QueryPerformanceCounter(&f1);
  std::printf("D3D12CreateDevice OK in %.1f ms\n", (f1.QuadPart - f0.QuadPart) * 1000.0 / freq.QuadPart);

  // Caps: the feature level the device admits and the tiers that decide it.
  D3D_FEATURE_LEVEL levels[] = { D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_12_0,
                                 D3D_FEATURE_LEVEL_12_1, (D3D_FEATURE_LEVEL)0xc200 /* 12_2 */ };
  D3D12_FEATURE_DATA_FEATURE_LEVELS fl = {};
  fl.NumFeatureLevels = 5;
  fl.pFeatureLevelsRequested = levels;
  if (SUCCEEDED(g.dev->CheckFeatureSupport(D3D12_FEATURE_FEATURE_LEVELS, &fl, sizeof(fl))))
    std::printf("caps: max feature level 0x%x\n", fl.MaxSupportedFeatureLevel);
  D3D12_FEATURE_DATA_SHADER_MODEL sm = { (D3D_SHADER_MODEL)0x69 };
  for (int s = 0x69; s >= 0x60; s--) {
    sm.HighestShaderModel = (D3D_SHADER_MODEL)s;
    if (SUCCEEDED(g.dev->CheckFeatureSupport(D3D12_FEATURE_SHADER_MODEL, &sm, sizeof(sm)))) break;
  }
  std::printf("caps: highest shader model 0x%x\n", sm.HighestShaderModel);
  D3D12_FEATURE_DATA_D3D12_OPTIONS o = {};
  if (SUCCEEDED(g.dev->CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS, &o, sizeof(o))))
    std::printf("caps: binding tier %s, tiled %s, conservative raster %s, ROVs %d, "
                "typed UAV load additional %d, heap tier %s\n",
                tier_name(o.ResourceBindingTier), tier_name(o.TiledResourcesTier),
                tier_name(o.ConservativeRasterizationTier), o.ROVsSupported,
                o.TypedUAVLoadAdditionalFormats, tier_name(o.ResourceHeapTier));
  D3D12_FEATURE_DATA_D3D12_OPTIONS5 o5 = {};
  if (SUCCEEDED(g.dev->CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS5, &o5, sizeof(o5))))
    std::printf("caps: raytracing tier %d, render passes tier %d\n", o5.RaytracingTier, o5.RenderPassesTier);
  // OPTIONS6/7 (feature ids 30/32): newer than MinGW's d3d12.h.
  struct { int MeshShaderTier, SamplerFeedbackTier; } o7 = {};
  if (SUCCEEDED(g.dev->CheckFeatureSupport((D3D12_FEATURE)32, &o7, sizeof(o7))))
    std::printf("caps: mesh shader tier %d, sampler feedback tier %d\n", o7.MeshShaderTier,
                o7.SamplerFeedbackTier);
  struct { BOOL AdditionalShadingRates, PerPrimitive; int VariableShadingRateTier; UINT TileSize; BOOL Bg; } o6 = {};
  if (SUCCEEDED(g.dev->CheckFeatureSupport((D3D12_FEATURE)30, &o6, sizeof(o6))))
    std::printf("caps: VRS tier %d\n", o6.VariableShadingRateTier);

  D3D12_COMMAND_QUEUE_DESC qd = {};
  qd.Type = D3D12_COMMAND_LIST_TYPE_DIRECT;
  CHECK(g.dev->CreateCommandQueue(&qd, __uuidof(ID3D12CommandQueue), (void**)&g.queue));
  CHECK(g.dev->CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT, __uuidof(ID3D12CommandAllocator),
                                      (void**)&g.alloc));
  CHECK(g.dev->CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, g.alloc, nullptr,
                                 __uuidof(ID3D12GraphicsCommandList), (void**)&g.list));
  CHECK(g.dev->CreateFence(0, D3D12_FENCE_FLAG_NONE, __uuidof(ID3D12Fence), (void**)&g.fence));
  g.event = CreateEventA(nullptr, FALSE, FALSE, nullptr);

  int failures = 0;

  // --- compute: 4096 uints through a root UAV, read back ---------------------
  {
    D3D12_ROOT_PARAMETER rp = {};
    rp.ParameterType = D3D12_ROOT_PARAMETER_TYPE_UAV;
    rp.ShaderVisibility = D3D12_SHADER_VISIBILITY_ALL;
    D3D12_ROOT_SIGNATURE_DESC rsd = {};
    rsd.NumParameters = 1;
    rsd.pParameters = &rp;
    ID3DBlob* rsb = nullptr;
    CHECK(serialize_rs(&rsd, D3D_ROOT_SIGNATURE_VERSION_1, &rsb, nullptr));
    ID3D12RootSignature* rs = nullptr;
    CHECK(g.dev->CreateRootSignature(0, rsb->GetBufferPointer(), rsb->GetBufferSize(),
                                     __uuidof(ID3D12RootSignature), (void**)&rs));
    ID3DBlob* cs = compile(kCompute, sizeof(kCompute) - 1, "cs", "cs_5_0");
    D3D12_COMPUTE_PIPELINE_STATE_DESC cpd = {};
    cpd.pRootSignature = rs;
    cpd.CS = { cs->GetBufferPointer(), cs->GetBufferSize() };
    ID3D12PipelineState* pso = nullptr;
    CHECK(g.dev->CreateComputePipelineState(&cpd, __uuidof(ID3D12PipelineState), (void**)&pso));
    const UINT n = 4096;
    ID3D12Resource* uav = buffer(g.dev, n * 4, D3D12_HEAP_TYPE_DEFAULT,
                                 D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
    ID3D12Resource* rb = buffer(g.dev, n * 4, D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_FLAG_NONE,
                                D3D12_RESOURCE_STATE_COPY_DEST);
    g.list->SetComputeRootSignature(rs);
    g.list->SetPipelineState(pso);
    g.list->SetComputeRootUnorderedAccessView(0, uav->GetGPUVirtualAddress());
    g.list->Dispatch(n / 64, 1, 1);
    D3D12_RESOURCE_BARRIER b = transition(uav, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE);
    g.list->ResourceBarrier(1, &b);
    g.list->CopyResource(rb, uav);
    g.submit_and_wait("compute");
    UINT* p = nullptr;
    D3D12_RANGE r = { 0, n * 4 };
    CHECK(rb->Map(0, &r, (void**)&p));
    UINT bad = 0, first_bad = ~0u, first_val = 0;
    for (UINT i = 0; i < n; i++)
      if (p[i] != i * 3 + 1) { if (!bad) { first_bad = i; first_val = p[i]; } bad++; }
    rb->Unmap(0, nullptr);
    if (bad) {
      std::printf("compute: FAIL %u of %u wrong (first [%u] = %u)\n", bad, n, first_bad, first_val);
      failures++;
    } else {
      std::printf("compute: OK (%u values)\n", n);
    }
    rb->Release(); uav->Release(); pso->Release(); rs->Release(); cs->Release(); rsb->Release();
  }

  // --- graphics pipeline (shared by the offscreen check and the swap chain) --
  D3D12_ROOT_PARAMETER grp = {};
  grp.ParameterType = D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS;
  grp.Constants.Num32BitValues = 4;
  grp.ShaderVisibility = D3D12_SHADER_VISIBILITY_VERTEX;
  D3D12_ROOT_SIGNATURE_DESC grsd = {};
  grsd.NumParameters = 1;
  grsd.pParameters = &grp;
  ID3DBlob* grsb = nullptr;
  CHECK(serialize_rs(&grsd, D3D_ROOT_SIGNATURE_VERSION_1, &grsb, nullptr));
  ID3D12RootSignature* grs = nullptr;
  CHECK(g.dev->CreateRootSignature(0, grsb->GetBufferPointer(), grsb->GetBufferSize(),
                                   __uuidof(ID3D12RootSignature), (void**)&grs));
  ID3DBlob* vs = compile(kGfx, sizeof(kGfx) - 1, "vs", "vs_5_0");
  ID3DBlob* ps = compile(kGfx, sizeof(kGfx) - 1, "ps", "ps_5_0");
  D3D12_GRAPHICS_PIPELINE_STATE_DESC gpd = {};
  gpd.pRootSignature = grs;
  gpd.VS = { vs->GetBufferPointer(), vs->GetBufferSize() };
  gpd.PS = { ps->GetBufferPointer(), ps->GetBufferSize() };
  gpd.BlendState.RenderTarget[0].RenderTargetWriteMask = D3D12_COLOR_WRITE_ENABLE_ALL;
  gpd.SampleMask = ~0u;
  gpd.RasterizerState.FillMode = D3D12_FILL_MODE_SOLID;
  gpd.RasterizerState.CullMode = D3D12_CULL_MODE_NONE;
  gpd.RasterizerState.DepthClipEnable = TRUE;
  gpd.PrimitiveTopologyType = D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE;
  gpd.NumRenderTargets = 1;
  gpd.RTVFormats[0] = DXGI_FORMAT_R8G8B8A8_UNORM;
  gpd.SampleDesc.Count = 1;
  ID3D12PipelineState* gpso = nullptr;
  CHECK(g.dev->CreateGraphicsPipelineState(&gpd, __uuidof(ID3D12PipelineState), (void**)&gpso));

  D3D12_DESCRIPTOR_HEAP_DESC hd = {};
  hd.Type = D3D12_DESCRIPTOR_HEAP_TYPE_RTV;
  hd.NumDescriptors = 8;
  ID3D12DescriptorHeap* rtv_heap = nullptr;
  CHECK(g.dev->CreateDescriptorHeap(&hd, __uuidof(ID3D12DescriptorHeap), (void**)&rtv_heap));
  const UINT rtv_step = g.dev->GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV);
  auto rtv = [&](UINT i) {
    D3D12_CPU_DESCRIPTOR_HANDLE h = rtv_heap->GetCPUDescriptorHandleForHeapStart();
    h.ptr += SIZE_T(i) * rtv_step;
    return h;
  };
  auto draw = [&](ID3D12Resource* target, UINT index, UINT w, UINT h, const float clear[4], const float rot[4]) {
    D3D12_CPU_DESCRIPTOR_HANDLE hh = rtv(index);
    g.list->OMSetRenderTargets(1, &hh, FALSE, nullptr);
    g.list->ClearRenderTargetView(hh, clear, 0, nullptr);
    D3D12_VIEWPORT vp = { 0, 0, float(w), float(h), 0, 1 };
    D3D12_RECT sc = { 0, 0, LONG(w), LONG(h) };
    g.list->RSSetViewports(1, &vp);
    g.list->RSSetScissorRects(1, &sc);
    g.list->SetGraphicsRootSignature(grs);
    g.list->SetPipelineState(gpso);
    g.list->SetGraphicsRoot32BitConstants(0, 4, rot, 0);
    g.list->IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
    g.list->DrawInstanced(3, 1, 0, 0);
    (void)target;
  };

  // --- offscreen: clear + flat triangle, read back two pixels ----------------
  {
    const UINT w = 256, h = 256;
    D3D12_HEAP_PROPERTIES hp = {};
    hp.Type = D3D12_HEAP_TYPE_DEFAULT;
    D3D12_RESOURCE_DESC td = {};
    td.Dimension = D3D12_RESOURCE_DIMENSION_TEXTURE2D;
    td.Width = w;
    td.Height = h;
    td.DepthOrArraySize = 1;
    td.MipLevels = 1;
    td.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    td.SampleDesc.Count = 1;
    td.Flags = D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET;
    D3D12_CLEAR_VALUE cv = {};
    cv.Format = td.Format;
    ID3D12Resource* tex = nullptr;
    CHECK(g.dev->CreateCommittedResource(&hp, D3D12_HEAP_FLAG_NONE, &td, D3D12_RESOURCE_STATE_RENDER_TARGET, &cv,
                                         __uuidof(ID3D12Resource), (void**)&tex));
    g.dev->CreateRenderTargetView(tex, nullptr, rtv(7));
    const UINT pitch = w * 4; // 1024: already 256-aligned
    ID3D12Resource* rb = buffer(g.dev, UINT64(pitch) * h, D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_FLAG_NONE,
                                D3D12_RESOURCE_STATE_COPY_DEST);
    const float clear[4] = { 0.0f, 0.0f, 1.0f, 1.0f };
    const float rot[4] = { 1, 0, 1, 1 };
    draw(tex, 7, w, h, clear, rot);
    D3D12_RESOURCE_BARRIER b = transition(tex, D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_COPY_SOURCE);
    g.list->ResourceBarrier(1, &b);
    D3D12_TEXTURE_COPY_LOCATION dst = {}, src = {};
    dst.pResource = rb;
    dst.Type = D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT;
    dst.PlacedFootprint.Footprint = { DXGI_FORMAT_R8G8B8A8_UNORM, w, h, 1, pitch };
    src.pResource = tex;
    src.Type = D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX;
    g.list->CopyTextureRegion(&dst, 0, 0, 0, &src, nullptr);
    g.submit_and_wait("offscreen");
    UINT* p = nullptr;
    D3D12_RANGE r = { 0, SIZE_T(pitch) * h };
    CHECK(rb->Map(0, &r, (void**)&p));
    const UINT center = p[(h / 2) * (pitch / 4) + w / 2];
    const UINT corner = p[2 * (pitch / 4) + 2];
    rb->Unmap(0, nullptr);
    // R8G8B8A8 little endian: 0xAABBGGRR.
    const bool ok = center == 0xff00ffffu && corner == 0xffff0000u;
    std::printf("offscreen: %s (center 0x%08x want 0xff00ffff, corner 0x%08x want 0xffff0000)\n",
                ok ? "OK" : "FAIL", center, corner);
    if (!ok) failures++;
    rb->Release();
    tex->Release();
  }

  if (offscreen) {
    std::printf("result: %s\n", failures ? "FAIL" : "PASS");
    return failures ? 1 : 0;
  }

  // --- flip-model swap chain: spinning triangle ------------------------------
  WNDCLASSA wc = {};
  wc.lpfnWndProc = wndproc;
  wc.hInstance = GetModuleHandleA(nullptr);
  wc.lpszClassName = "d3d12_tri";
  wc.hCursor = LoadCursor(nullptr, IDC_ARROW);
  RegisterClassA(&wc);
  RECT rc = { 0, 0, LONG(width), LONG(height) };
  AdjustWindowRect(&rc, WS_OVERLAPPEDWINDOW, FALSE);
  HWND hwnd = CreateWindowA("d3d12_tri", "d3d12_tri", WS_OVERLAPPEDWINDOW | WS_VISIBLE, 40, 40,
                            rc.right - rc.left, rc.bottom - rc.top, nullptr, nullptr, wc.hInstance, nullptr);
  const UINT kBuffers = 3;
  DXGI_SWAP_CHAIN_DESC1 sd = {};
  sd.Width = width;
  sd.Height = height;
  sd.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
  sd.SampleDesc.Count = 1;
  sd.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
  sd.BufferCount = kBuffers;
  sd.SwapEffect = DXGI_SWAP_EFFECT_FLIP_DISCARD;
  IDXGISwapChain1* sc1 = nullptr;
  CHECK(factory->CreateSwapChainForHwnd(g.queue, hwnd, &sd, nullptr, nullptr, &sc1));
  IDXGISwapChain3* sc = nullptr;
  CHECK(sc1->QueryInterface(__uuidof(IDXGISwapChain3), (void**)&sc));
  std::printf("swapchain: %ux%u x%u FLIP_DISCARD R8G8B8A8, sync %u\n", width, height, kBuffers, sync);
  ID3D12Resource* bb[kBuffers] = {};
  for (UINT i = 0; i < kBuffers; i++) {
    CHECK(sc->GetBuffer(i, __uuidof(ID3D12Resource), (void**)&bb[i]));
    g.dev->CreateRenderTargetView(bb[i], nullptr, rtv(i));
  }
  // One allocator per frame in flight, fenced.
  ID3D12CommandAllocator* fa[kBuffers] = {};
  UINT64 fv[kBuffers] = {};
  for (UINT i = 0; i < kBuffers; i++)
    CHECK(g.dev->CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT, __uuidof(ID3D12CommandAllocator),
                                        (void**)&fa[i]));
  CHECK(g.list->Close());

  LARGE_INTEGER start, now, last;
  QueryPerformanceCounter(&start);
  last = start;
  unsigned frames = 0, total = 0, present_fail = 0;
  double fps_sum = 0;
  int fps_n = 0;
  MSG msg;
  for (;;) {
    while (PeekMessageA(&msg, nullptr, 0, 0, PM_REMOVE)) {
      if (msg.message == WM_QUIT) goto done;
      TranslateMessage(&msg);
      DispatchMessageA(&msg);
    }
    QueryPerformanceCounter(&now);
    const double t = double(now.QuadPart - start.QuadPart) / freq.QuadPart;
    if (t >= seconds) break;
    const UINT i = sc->GetCurrentBackBufferIndex();
    if (g.fence->GetCompletedValue() < fv[i]) {
      CHECK(g.fence->SetEventOnCompletion(fv[i], g.event));
      if (WaitForSingleObject(g.event, 5000) != WAIT_OBJECT_0) {
        std::printf("FAIL: frame fence %llu not reached in 5 s\n", (unsigned long long)fv[i]);
        return 3;
      }
    }
    CHECK(fa[i]->Reset());
    CHECK(g.list->Reset(fa[i], nullptr));
    D3D12_RESOURCE_BARRIER b = transition(bb[i], D3D12_RESOURCE_STATE_PRESENT, D3D12_RESOURCE_STATE_RENDER_TARGET);
    g.list->ResourceBarrier(1, &b);
    const float clear[4] = { 0.1f, 0.1f, 0.15f + 0.1f * float(std::sin(t)), 1.0f };
    const float rot[4] = { float(std::cos(t * 2)), float(std::sin(t * 2)), float(height) / float(width), 0 };
    draw(bb[i], i, width, height, clear, rot);
    b = transition(bb[i], D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_PRESENT);
    g.list->ResourceBarrier(1, &b);
    CHECK(g.list->Close());
    ID3D12CommandList* lists[] = { g.list };
    g.queue->ExecuteCommandLists(1, lists);
    HRESULT phr = sc->Present(sync, 0);
    if (FAILED(phr)) {
      if (present_fail++ < 4) std::printf("Present failed 0x%08lx\n", (unsigned long)phr);
    }
    CHECK(g.queue->Signal(g.fence, ++g.value));
    fv[i] = g.value;
    frames++;
    total++;
    const double dt = double(now.QuadPart - last.QuadPart) / freq.QuadPart;
    if (dt >= 1.0) {
      std::printf("fps %.1f\n", frames / dt);
      if (total > frames) { fps_sum += frames / dt; fps_n++; }
      frames = 0;
      last = now;
    }
  }
done:
  // Drain.
  CHECK(g.queue->Signal(g.fence, ++g.value));
  CHECK(g.fence->SetEventOnCompletion(g.value, g.event));
  WaitForSingleObject(g.event, 5000);
  std::printf("frames %u, average fps %.1f (excluding the first second), present failures %u\n", total,
              fps_n ? fps_sum / fps_n : 0.0, present_fail);
  if (present_fail) failures++;
  std::printf("result: %s\n", failures ? "FAIL" : "PASS");
  for (UINT i = 0; i < kBuffers; i++) { bb[i]->Release(); fa[i]->Release(); }
  sc->Release();
  sc1->Release();
  DestroyWindow(hwnd);
  return failures ? 1 : 0;
}
