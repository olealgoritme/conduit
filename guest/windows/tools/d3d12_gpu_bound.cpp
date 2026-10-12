// d3d12_gpu_bound: GPU-bound throughput per stage through D3D12, in GPU time
// from timestamp queries. The D3D12 twin of guest/nvk-rm/tests/perf/
// vk_gpu_bound.c: the same tests with the same names and the same work, so
// the D3D12 path (the Helios D3D12 UMD + vkd3d-proton on NVK) can be held
// against Vulkan on NVK in the same guest, test by test. The tests that only
// D3D12 has (rootcbv) measure what D3D12 apps do on every draw.
//
//   d3d12_gpu_bound.exe [-r reps] [-t test,...] [-w width -h height] [-s seconds]
//
// Offscreen (no swap chain): runs from an ssh session. d3d12.dll is whatever
// the loader resolves (the system runtime: the Helios UMD on the Helios
// adapter). Adapter 0 unless BENCH_ADAPTER=<index>.
//
// Tests (default: all, in this order; sustain last):
//   alu      transcendental + FMA loop, fullscreen x4             Gpix/s
//   fma      4 independent float4 FMA chains, fullscreen x4       TFLOP/s
//   fill     16 fullscreen passes, RGBA8 constant / RGBA8 per-pixel noise /
//            RGBA16F blended, into a committed and into a placed render
//            target (a heap, as engines place their targets)       Gpix/s
//   tex      8 bilinear taps per pixel: 4096^2 RGBA8, cached and streaming;
//            8192^2 (256 MiB) read at a random place per 8x8 block Gtap/s
//   bw       compute copy of 256 MiB                              GB/s
//   barrier  256 dependent small dispatches with a UAV barrier on the
//            resource, a UAV barrier on all (null) or none        us/barrier
//   rtpass   256 960x540 draws, each reading the previous one's target,
//            RT <-> PIXEL_SHADER_RESOURCE transitions between them, against
//            the same draws with no barrier                       us/pass
//   rootcbv  20000 small draws: one root CBV for all, a new root CBV in an
//            UPLOAD heap per draw, a new one in a DEFAULT heap per draw
//                                                                 Mdraws/s
//   record   CPU cost of recording: 8 lists x 64 dispatches per frame with
//            allocators reused, then new allocators every frame   us/dispatch
//   submit   the same GPU work per frame (64 units of ~50 us) in 1/4/16/64
//            ExecuteCommandLists, one Signal per frame, 2 frames in flight:
//            wall time vs GPU busy, the idle gap per ExecuteCommandLists
//   sustain  ~4 ms ALU ExecuteCommandLists back to back for -s seconds
//            (default 10): GPU time per batch over time
//
// Build (MinGW-w64 on Linux):
//   x86_64-w64-mingw32-g++ -O2 -static d3d12_gpu_bound.cpp -o d3d12_gpu_bound.exe
//       -ldxgi -ld3dcompiler -luuid
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d12.h>
#include <d3dcompiler.h>
#include <dxgi1_4.h>

#include <algorithm>
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <vector>

#define CHECK(expr)                                                                   \
  do {                                                                                \
    HRESULT hr_ = (expr);                                                             \
    if (FAILED(hr_)) {                                                                \
      std::printf("FAIL %s:%d %s -> 0x%08lx\n", __FILE__, __LINE__, #expr, (unsigned long)hr_); \
      std::fflush(stdout);                                                            \
      std::exit(2);                                                                   \
    }                                                                                 \
  } while (0)

typedef HRESULT(WINAPI* PfnCreateDevice)(IUnknown*, D3D_FEATURE_LEVEL, REFIID, void**);
typedef HRESULT(WINAPI* PfnSerializeRs)(const D3D12_ROOT_SIGNATURE_DESC*, D3D_ROOT_SIGNATURE_VERSION,
                                        ID3DBlob**, ID3DBlob**);

static UINT W = 3840, H = 2160;
static int reps = 11;
static double sustain_s = 10.0;

// The same work as vk_gpu_bound's GLSL (gen_gpu_bound_shaders.py). Root
// constants b0 are its push constants.
static const char kShaders[] = R"(
cbuffer PC : register(b0) { float4 p; uint4 u; };
cbuffer Draw : register(b1) { float4 dpos; float4 dcol; };
Texture2D<float4> t : register(t0);
SamplerState smp : register(s0);
RWStructuredBuffer<uint4> dst : register(u0);
RWStructuredBuffer<uint4> src : register(u1);

uint pcg(uint v) {
  uint s = v * 747796405u + 2891336453u;
  uint w = ((s >> ((s >> 28u) + 4u)) ^ s) * 277803737u;
  return (w >> 22u) ^ w;
}
float4 unpack8(uint h) {
  return float4(h & 0xff, (h >> 8) & 0xff, (h >> 16) & 0xff, h >> 24) / 255.0;
}

float4 vs_fsq(uint id : SV_VertexID) : SV_Position {
  float2 q = float2((id << 1) & 2, id & 2);
  return float4(q * 2.0 - 1.0, 0.0, 1.0);
}
float4 vs_quad(uint id : SV_VertexID) : SV_Position {
  float2 q = float2(id & 1, id >> 1) * 0.02;
  return float4(q + dpos.xy, 0.0, 1.0);
}
float4 ps_alu(float4 fc : SV_Position) : SV_Target {
  float4 a = float4(fc.xy * 0.001, p.x, 1.0);
  for (uint i = 0; i < u.x; i++) {
    a = frac(a * a.yzwx * 1.0001 + float4(0.1, 0.2, 0.3, 0.4));
    a = sin(a) * 0.5 + a;
  }
  return a;
}
float4 ps_fma(float4 fc : SV_Position) : SV_Target {
  float4 a = fc.xyxy * 0.001;
  float4 b = a + 0.1, c = a + 0.2, d = a + 0.3;
  float4 k = p * 0.5 + 0.25, m = p * 0.125;
  for (uint i = 0; i < u.x; i++) {
    a = mad(a, k, m); b = mad(b, k, m);
    c = mad(c, k, m); d = mad(d, k, m);
  }
  return a + b + c + d;
}
float4 ps_color(float4 fc : SV_Position) : SV_Target { return p; }
float4 ps_noise(float4 fc : SV_Position) : SV_Target {
  uint2 c = uint2(fc.xy);
  return unpack8(pcg(c.x ^ (c.y << 16) ^ (u.y * 0x9e3779b9u)));
}
float4 ps_tex(float4 fc : SV_Position) : SV_Target {
  float2 uv = fc.xy * p.xy;
  float4 s = 0;
  [unroll] for (int i = 0; i < 8; i++)
    s += t.SampleLevel(smp, uv + float2(i, i) * p.zw, 0);
  return s;
}
float4 ps_texrand(float4 fc : SV_Position) : SV_Target {
  uint2 c = uint2(fc.xy);
  uint h = pcg((c.x >> 3u) ^ ((c.y >> 3u) << 12u) ^ (u.y * 0x9e3779b9u));
  float2 base = float2(h & 0xffffu, h >> 16u) / 65536.0;
  float4 s = 0;
  [unroll] for (int i = 0; i < 4; i++)
    s += t.SampleLevel(smp, base + (float2(c & 7u) + float2(i * 8, 0)) * p.xy, 0);
  return s;
}
float4 ps_blit(float4 fc : SV_Position) : SV_Target {
  return t.SampleLevel(smp, fc.xy * p.xy, 0) * 0.999 + 0.001;
}
float4 ps_cb(float4 fc : SV_Position) : SV_Target { return dcol; }

[numthreads(256, 1, 1)]
void cs_copy(uint3 id : SV_DispatchThreadID) {
  if (id.x < u.x) dst[id.x] = src[id.x];
}
[numthreads(64, 1, 1)]
void cs_spin(uint3 id : SV_DispatchThreadID) {
  float4 a = float4(id.x * 1e-6, 0.1, 0.2, 0.3);
  for (uint k = 0; k < u.x; k++)
    a = mad(a, 1.0001, 1e-4);
  dst[id.x & 0xffffu] = asuint(a);
}
)";

struct PC {
  float p[4];
  UINT u[4];
};

static double now_ms() {
  static LARGE_INTEGER f;
  LARGE_INTEGER c;
  if (!f.QuadPart) QueryPerformanceFrequency(&f);
  QueryPerformanceCounter(&c);
  return (double)c.QuadPart * 1e3 / (double)f.QuadPart;
}

static double pct(std::vector<double> v, double p) {
  if (v.empty()) return 0;
  std::sort(v.begin(), v.end());
  size_t i = (size_t)(p * (v.size() - 1) + 0.5);
  return v[std::min(i, v.size() - 1)];
}

static ID3D12Device* dev;
static ID3D12CommandQueue* queue;
static ID3D12Fence* fence;
static UINT64 fence_value;
static HANDLE fence_event;
static ID3D12RootSignature* rs;
static ID3D12QueryHeap* qheap;
static ID3D12Resource* qreadback;
static UINT64* qmap;
static double ts_period_ms;
static ID3D12DescriptorHeap* srv_heap;  // shader visible
static ID3D12DescriptorHeap* rtv_heap;
static UINT srv_inc, rtv_inc;
static const UINT NQUERIES = 1024;

enum { RP_CONST = 0, RP_TABLE = 1, RP_UAV0 = 2, RP_UAV1 = 3, RP_CBV = 4 };

static ID3DBlob* compile(const char* entry, const char* target) {
  ID3DBlob* code = nullptr;
  ID3DBlob* err = nullptr;
  if (FAILED(D3DCompile(kShaders, sizeof(kShaders) - 1, "gpu_bound", nullptr, nullptr, entry, target,
                        D3DCOMPILE_OPTIMIZATION_LEVEL3, 0, &code, &err))) {
    std::printf("D3DCompile %s: %s\n", entry, err ? (const char*)err->GetBufferPointer() : "?");
    std::exit(2);
  }
  return code;
}

static void wait_value(UINT64 v) {
  if (fence->GetCompletedValue() >= v) return;
  CHECK(fence->SetEventOnCompletion(v, fence_event));
  if (WaitForSingleObject(fence_event, 10000) != WAIT_OBJECT_0) {
    std::printf("FAIL: fence %llu not reached in 10 s (completed %llu)\n", (unsigned long long)v,
                (unsigned long long)fence->GetCompletedValue());
    std::exit(3);
  }
}

static UINT64 signal() {
  CHECK(queue->Signal(fence, ++fence_value));
  return fence_value;
}

static void execute(ID3D12GraphicsCommandList* l) {
  ID3D12CommandList* lists[] = { l };
  queue->ExecuteCommandLists(1, lists);
}

struct Cmd {
  ID3D12CommandAllocator* alloc = nullptr;
  ID3D12GraphicsCommandList* list = nullptr;
  void begin() {
    if (!alloc) {
      CHECK(dev->CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT, __uuidof(ID3D12CommandAllocator),
                                        (void**)&alloc));
      CHECK(dev->CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, alloc, nullptr,
                                   __uuidof(ID3D12GraphicsCommandList), (void**)&list));
    } else {
      CHECK(alloc->Reset());
      CHECK(list->Reset(alloc, nullptr));
    }
    ID3D12DescriptorHeap* heaps[] = { srv_heap };
    list->SetDescriptorHeaps(1, heaps);
    list->SetGraphicsRootSignature(rs);
    list->SetComputeRootSignature(rs);
  }
  void close() { CHECK(list->Close()); }
  void release() {
    if (list) list->Release();
    if (alloc) alloc->Release();
    list = nullptr;
    alloc = nullptr;
  }
};

static void run_wait(Cmd& c) {
  execute(c.list);
  wait_value(signal());
}

static void ts(ID3D12GraphicsCommandList* l, UINT q) { l->EndQuery(qheap, D3D12_QUERY_TYPE_TIMESTAMP, q); }
static void resolve(ID3D12GraphicsCommandList* l, UINT first, UINT count) {
  l->ResolveQueryData(qheap, D3D12_QUERY_TYPE_TIMESTAMP, first, count, qreadback, first * 8ull);
}
static double ts_ms(UINT64 a, UINT64 b) { return (double)(b - a) * ts_period_ms; }

static D3D12_RESOURCE_BARRIER transition(ID3D12Resource* r, D3D12_RESOURCE_STATES a, D3D12_RESOURCE_STATES b) {
  D3D12_RESOURCE_BARRIER t = {};
  t.Type = D3D12_RESOURCE_BARRIER_TYPE_TRANSITION;
  t.Transition.pResource = r;
  t.Transition.Subresource = D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES;
  t.Transition.StateBefore = a;
  t.Transition.StateAfter = b;
  return t;
}

static void report(const char* name, double gpu_ms, double work, const char* unit) {
  if (work > 0)
    std::printf("%-34s gpu %9.3f ms  %10.2f %s\n", name, gpu_ms, work / (gpu_ms * 1e-3), unit);
  else
    std::printf("%-34s gpu %9.3f ms\n", name, gpu_ms);
  std::fflush(stdout);
}

// Records body between timestamps 0 and 1, executes it reps times after a
// warm-up (a closed list can be executed again once it completed) and
// returns the median GPU time.
template <typename F>
static double time_body(F body) {
  Cmd c;
  c.begin();
  ts(c.list, 0);
  body(c.list);
  ts(c.list, 1);
  resolve(c.list, 0, 2);
  c.close();
  run_wait(c);
  std::vector<double> gpu;
  int n = std::min(reps, 64);
  for (int i = 0; i < n; i++) {
    run_wait(c);
    gpu.push_back(ts_ms(qmap[0], qmap[1]));
  }
  c.release();
  return pct(gpu, 0.5);
}

// ---------------------------------------------------------------------------
// resources

static ID3D12Resource* buffer(UINT64 size, D3D12_HEAP_TYPE type, D3D12_RESOURCE_FLAGS flags,
                              D3D12_RESOURCE_STATES state) {
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
  CHECK(dev->CreateCommittedResource(&hp, D3D12_HEAP_FLAG_NONE, &rd, state, nullptr, __uuidof(ID3D12Resource),
                                     (void**)&r));
  return r;
}

static D3D12_RESOURCE_DESC tex_desc(DXGI_FORMAT fmt, UINT w, UINT h, D3D12_RESOURCE_FLAGS flags) {
  D3D12_RESOURCE_DESC rd = {};
  rd.Dimension = D3D12_RESOURCE_DIMENSION_TEXTURE2D;
  rd.Width = w;
  rd.Height = h;
  rd.DepthOrArraySize = 1;
  rd.MipLevels = 1;
  rd.Format = fmt;
  rd.SampleDesc.Count = 1;
  rd.Layout = D3D12_TEXTURE_LAYOUT_UNKNOWN;
  rd.Flags = flags;
  return rd;
}

// Render targets in a heap of their own, as engines place theirs
static ID3D12Heap* rt_heap;
static UINT64 rt_heap_off;

static ID3D12Resource* texture(DXGI_FORMAT fmt, UINT w, UINT h, D3D12_RESOURCE_FLAGS flags,
                               D3D12_RESOURCE_STATES state, bool placed) {
  D3D12_RESOURCE_DESC rd = tex_desc(fmt, w, h, flags);
  D3D12_CLEAR_VALUE cv = {};
  cv.Format = fmt;
  const bool rt = flags & D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET;
  ID3D12Resource* r = nullptr;
  if (!placed) {
    D3D12_HEAP_PROPERTIES hp = {};
    hp.Type = D3D12_HEAP_TYPE_DEFAULT;
    CHECK(dev->CreateCommittedResource(&hp, D3D12_HEAP_FLAG_NONE, &rd, state, rt ? &cv : nullptr,
                                       __uuidof(ID3D12Resource), (void**)&r));
    return r;
  }
  if (!rt_heap) {
    D3D12_HEAP_DESC hd = {};
    hd.SizeInBytes = 512ull << 20;
    hd.Properties.Type = D3D12_HEAP_TYPE_DEFAULT;
    hd.Alignment = D3D12_DEFAULT_RESOURCE_PLACEMENT_ALIGNMENT;
    hd.Flags = D3D12_HEAP_FLAG_ALLOW_ONLY_RT_DS_TEXTURES;
    CHECK(dev->CreateHeap(&hd, __uuidof(ID3D12Heap), (void**)&rt_heap));
  }
  D3D12_RESOURCE_ALLOCATION_INFO ai = dev->GetResourceAllocationInfo(0, 1, &rd);
  UINT64 off = (rt_heap_off + ai.Alignment - 1) & ~(ai.Alignment - 1);
  if (off + ai.SizeInBytes > (512ull << 20)) {
    std::printf("FAIL: render target heap full\n");
    std::exit(2);
  }
  CHECK(dev->CreatePlacedResource(rt_heap, off, &rd, state, rt ? &cv : nullptr, __uuidof(ID3D12Resource),
                                  (void**)&r));
  rt_heap_off = off + ai.SizeInBytes;
  return r;
}

static D3D12_CPU_DESCRIPTOR_HANDLE rtv(UINT i) {
  D3D12_CPU_DESCRIPTOR_HANDLE h = rtv_heap->GetCPUDescriptorHandleForHeapStart();
  h.ptr += (SIZE_T)i * rtv_inc;
  return h;
}
static D3D12_GPU_DESCRIPTOR_HANDLE srv_gpu(UINT i) {
  D3D12_GPU_DESCRIPTOR_HANDLE h = srv_heap->GetGPUDescriptorHandleForHeapStart();
  h.ptr += (UINT64)i * srv_inc;
  return h;
}
static void make_rtv(ID3D12Resource* r, UINT i) { dev->CreateRenderTargetView(r, nullptr, rtv(i)); }
static void make_srv(ID3D12Resource* r, UINT i) {
  D3D12_CPU_DESCRIPTOR_HANDLE h = srv_heap->GetCPUDescriptorHandleForHeapStart();
  h.ptr += (SIZE_T)i * srv_inc;
  dev->CreateShaderResourceView(r, nullptr, h);
}

// Fills a texture with a clear through a temporary RTV (needs RT usage) or
// leaves it as created; then transitions it to `to`.
static void init_tex(ID3D12Resource* r, D3D12_RESOURCE_STATES from, D3D12_RESOURCE_STATES to, bool clear) {
  Cmd c;
  c.begin();
  if (clear) {
    make_rtv(r, 15);
    const float col[4] = { 0.25f, 0.5f, 0.75f, 1.0f };
    if (from != D3D12_RESOURCE_STATE_RENDER_TARGET) {
      D3D12_RESOURCE_BARRIER b = transition(r, from, D3D12_RESOURCE_STATE_RENDER_TARGET);
      c.list->ResourceBarrier(1, &b);
      from = D3D12_RESOURCE_STATE_RENDER_TARGET;
    }
    c.list->ClearRenderTargetView(rtv(15), col, 0, nullptr);
  }
  if (from != to) {
    D3D12_RESOURCE_BARRIER b = transition(r, from, to);
    c.list->ResourceBarrier(1, &b);
  }
  c.close();
  run_wait(c);
  c.release();
}

// ---------------------------------------------------------------------------
// pipelines

static ID3D12PipelineState* gfx(const char* ps, DXGI_FORMAT fmt, bool blend, const char* vs = "vs_fsq") {
  static ID3DBlob* vsb[2];
  const int vi = std::strcmp(vs, "vs_fsq") ? 1 : 0;
  if (!vsb[vi]) vsb[vi] = compile(vs, "vs_5_0");
  ID3DBlob* psb = compile(ps, "ps_5_0");
  D3D12_GRAPHICS_PIPELINE_STATE_DESC d = {};
  d.pRootSignature = rs;
  d.VS = { vsb[vi]->GetBufferPointer(), vsb[vi]->GetBufferSize() };
  d.PS = { psb->GetBufferPointer(), psb->GetBufferSize() };
  d.BlendState.RenderTarget[0].BlendEnable = blend;
  d.BlendState.RenderTarget[0].SrcBlend = D3D12_BLEND_SRC_ALPHA;
  d.BlendState.RenderTarget[0].DestBlend = D3D12_BLEND_INV_SRC_ALPHA;
  d.BlendState.RenderTarget[0].BlendOp = D3D12_BLEND_OP_ADD;
  d.BlendState.RenderTarget[0].SrcBlendAlpha = D3D12_BLEND_ONE;
  d.BlendState.RenderTarget[0].DestBlendAlpha = D3D12_BLEND_ZERO;
  d.BlendState.RenderTarget[0].BlendOpAlpha = D3D12_BLEND_OP_ADD;
  d.BlendState.RenderTarget[0].RenderTargetWriteMask = D3D12_COLOR_WRITE_ENABLE_ALL;
  d.SampleMask = 0xffffffff;
  d.RasterizerState.FillMode = D3D12_FILL_MODE_SOLID;
  d.RasterizerState.CullMode = D3D12_CULL_MODE_NONE;
  d.RasterizerState.DepthClipEnable = TRUE;
  d.PrimitiveTopologyType = D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE;
  d.NumRenderTargets = 1;
  d.RTVFormats[0] = fmt;
  d.SampleDesc.Count = 1;
  ID3D12PipelineState* p = nullptr;
  CHECK(dev->CreateGraphicsPipelineState(&d, __uuidof(ID3D12PipelineState), (void**)&p));
  psb->Release();
  return p;
}

static ID3D12PipelineState* comp(const char* cs) {
  ID3DBlob* b = compile(cs, "cs_5_0");
  D3D12_COMPUTE_PIPELINE_STATE_DESC d = {};
  d.pRootSignature = rs;
  d.CS = { b->GetBufferPointer(), b->GetBufferSize() };
  ID3D12PipelineState* p = nullptr;
  CHECK(dev->CreateComputePipelineState(&d, __uuidof(ID3D12PipelineState), (void**)&p));
  b->Release();
  return p;
}

static void set_target(ID3D12GraphicsCommandList* l, UINT rtv_index, UINT w, UINT h) {
  D3D12_CPU_DESCRIPTOR_HANDLE h0 = rtv(rtv_index);
  l->OMSetRenderTargets(1, &h0, FALSE, nullptr);
  D3D12_VIEWPORT vp = { 0, 0, (float)w, (float)h, 0, 1 };
  D3D12_RECT sc = { 0, 0, (LONG)w, (LONG)h };
  l->RSSetViewports(1, &vp);
  l->RSSetScissorRects(1, &sc);
  l->IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
}

static void push(ID3D12GraphicsCommandList* l, const PC& pc, bool compute = false) {
  if (compute)
    l->SetComputeRoot32BitConstants(RP_CONST, 8, &pc, 0);
  else
    l->SetGraphicsRoot32BitConstants(RP_CONST, 8, &pc, 0);
}

// Fullscreen passes into the target behind rtv_index (cleared first)
static double run_fs(ID3D12PipelineState* p, UINT rtv_index, int srv_index, int passes, PC pc, bool vary) {
  return time_body([&](ID3D12GraphicsCommandList* l) {
    const float black[4] = { 0, 0, 0, 0 };
    l->ClearRenderTargetView(rtv(rtv_index), black, 0, nullptr);
    set_target(l, rtv_index, W, H);
    l->SetPipelineState(p);
    if (srv_index >= 0) l->SetGraphicsRootDescriptorTable(RP_TABLE, srv_gpu(srv_index));
    for (int i = 0; i < passes; i++) {
      if (i == 0 || vary) {
        pc.u[1] = i;
        push(l, pc);
      }
      l->DrawInstanced(3, 1, 0, 0);
    }
  });
}

// ---------------------------------------------------------------------------
// tests

static void test_alu(bool alu, bool fma) {
  ID3D12Resource* rt = texture(DXGI_FORMAT_R8G8B8A8_UNORM, W, H, D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                               D3D12_RESOURCE_STATE_RENDER_TARGET, false);
  make_rtv(rt, 0);
  const double px = (double)W * H;
  if (alu) {
    ID3D12PipelineState* p = gfx("ps_alu", DXGI_FORMAT_R8G8B8A8_UNORM, false);
    PC pc = { { 0.5f, 0, 0, 0 }, { 64, 0, 0, 0 } };
    report("alu 64 iter x4", run_fs(p, 0, -1, 4, pc, false), px * 4 / 1e9, "Gpix/s");
    p->Release();
  }
  if (fma) {
    ID3D12PipelineState* p = gfx("ps_fma", DXGI_FORMAT_R8G8B8A8_UNORM, false);
    PC pc = { { 0.5f, 0.25f, 0.75f, 0.125f }, { 256, 0, 0, 0 } };
    report("fma 256 iter x4", run_fs(p, 0, -1, 4, pc, false), px * 4 * 256 * 32 / 1e12, "TFLOP/s");
    p->Release();
  }
  rt->Release();
}

static void test_fill() {
  const double px = (double)W * H;
  ID3D12PipelineState* p8c = gfx("ps_color", DXGI_FORMAT_R8G8B8A8_UNORM, false);
  ID3D12PipelineState* p8n = gfx("ps_noise", DXGI_FORMAT_R8G8B8A8_UNORM, false);
  ID3D12PipelineState* p16b = gfx("ps_noise", DXGI_FORMAT_R16G16B16A16_FLOAT, true);
  for (int placed = 0; placed <= 1; placed++) {
    // committed here is vk_gpu_bound's "dedicated" line
    const char* where = placed ? "placed" : "committed";
    ID3D12Resource* rt8 = texture(DXGI_FORMAT_R8G8B8A8_UNORM, W, H, D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                                  D3D12_RESOURCE_STATE_RENDER_TARGET, placed);
    ID3D12Resource* rt16 = texture(DXGI_FORMAT_R16G16B16A16_FLOAT, W, H, D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                                   D3D12_RESOURCE_STATE_RENDER_TARGET, placed);
    make_rtv(rt8, 0);
    make_rtv(rt16, 1);
    char name[64];
    PC pc = { { 0.2f, 0.4f, 0.6f, 0.5f }, { 0 } };
    std::snprintf(name, sizeof(name), "fill rgba8 const %s", where);
    report(name, run_fs(p8c, 0, -1, 16, pc, false), px * 16 / 1e9, "Gpix/s");
    std::snprintf(name, sizeof(name), "fill rgba8 noise %s", where);
    report(name, run_fs(p8n, 0, -1, 16, pc, true), px * 16 / 1e9, "Gpix/s");
    std::snprintf(name, sizeof(name), "blend rgba16f noise %s", where);
    report(name, run_fs(p16b, 1, -1, 16, pc, true), px * 16 / 1e9, "Gpix/s");
    rt8->Release();
    rt16->Release();
  }
  rt_heap_off = 0;
  p8c->Release();
  p8n->Release();
  p16b->Release();
}

static void test_tex() {
  const double px = (double)W * H;
  ID3D12Resource* rt = texture(DXGI_FORMAT_R8G8B8A8_UNORM, W, H, D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                               D3D12_RESOURCE_STATE_RENDER_TARGET, false);
  make_rtv(rt, 0);
  // Sampled-only textures, as engines upload theirs: never compressed. Their
  // contents do not matter to the sampler's throughput, so they are not
  // filled.
  ID3D12Resource* t4k = texture(DXGI_FORMAT_R8G8B8A8_UNORM, 4096, 4096, D3D12_RESOURCE_FLAG_NONE,
                                D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE, false);
  ID3D12Resource* t8k = texture(DXGI_FORMAT_R8G8B8A8_UNORM, 8192, 8192, D3D12_RESOURCE_FLAG_NONE,
                                D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE, false);
  make_srv(t4k, 0);
  make_srv(t8k, 1);
  ID3D12PipelineState* pt = gfx("ps_tex", DXGI_FORMAT_R8G8B8A8_UNORM, false);
  ID3D12PipelineState* pr = gfx("ps_texrand", DXGI_FORMAT_R8G8B8A8_UNORM, false);
  PC pc = { { 1.0f / 4096 / 4, 1.0f / 4096 / 4, 0.5f / 4096, 0.5f / 4096 }, { 0 } };
  report("tex 8 taps 4k rgba8 cached x4", run_fs(pt, 0, 0, 4, pc, false), px * 4 * 8 / 1e9, "Gtap/s");
  pc = { { 1.6f / 4096, 1.6f / 4096, 64.0f / 4096, 0 }, { 0 } };
  report("tex 8 taps 4k rgba8 stream x4", run_fs(pt, 0, 0, 4, pc, false), px * 4 * 8 / 1e9, "Gtap/s");
  pc = { { 1.0f / 8192, 1.0f / 8192, 0, 0 }, { 0 } };
  report("tex random 8x8 blocks 256MiB x4", run_fs(pr, 0, 1, 4, pc, true), px * 4 * 4 / 1e9, "Gtap/s");
  pt->Release();
  pr->Release();
  rt->Release();
  t4k->Release();
  t8k->Release();
}

static ID3D12PipelineState* p_copy;
static ID3D12PipelineState* p_spin;

static void test_bw() {
  const UINT64 size = 256ull << 20;
  ID3D12Resource* a = buffer(size, D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                             D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
  ID3D12Resource* b = buffer(size, D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                             D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
  const UINT n = (UINT)(size / 16);
  double t = time_body([&](ID3D12GraphicsCommandList* l) {
    l->SetPipelineState(p_copy);
    PC pc = { { 0 }, { n, 0, 0, 0 } };
    push(l, pc, true);
    l->SetComputeRootUnorderedAccessView(RP_UAV0, b->GetGPUVirtualAddress());
    l->SetComputeRootUnorderedAccessView(RP_UAV1, a->GetGPUVirtualAddress());
    for (int i = 0; i < 4; i++) l->Dispatch((n + 255) / 256, 1, 1);
  });
  report("copy 256 MiB compute x4", t, 2.0 * size * 4 / 1e9, "GB/s");
  a->Release();
  b->Release();
}

static void test_barrier() {
  const UINT64 size = 4ull << 20;
  ID3D12Resource* buf[2];
  for (auto& b : buf)
    b = buffer(size, D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
               D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
  const UINT n = (UINT)(size / 16);
  const int k = 256;
  auto run = [&](int mode) {
    return time_body([&](ID3D12GraphicsCommandList* l) {
      l->SetPipelineState(p_copy);
      PC pc = { { 0 }, { n, 0, 0, 0 } };
      push(l, pc, true);
      for (int i = 0; i < k; i++) {
        ID3D12Resource* d = buf[(i + 1) & 1];
        l->SetComputeRootUnorderedAccessView(RP_UAV0, d->GetGPUVirtualAddress());
        l->SetComputeRootUnorderedAccessView(RP_UAV1, buf[i & 1]->GetGPUVirtualAddress());
        l->Dispatch((n + 255) / 256, 1, 1);
        if (mode) {
          D3D12_RESOURCE_BARRIER b = {};
          b.Type = D3D12_RESOURCE_BARRIER_TYPE_UAV;
          b.UAV.pResource = mode == 1 ? d : nullptr;
          l->ResourceBarrier(1, &b);
        }
      }
    });
  };
  double t0 = run(0), t1 = run(1), t2 = run(2);
  report("barrier: 256 x 4 MiB copy, none", t0, 0, "");
  std::printf("%-34s gpu %9.3f ms  %10.2f us/barrier\n", "barrier: UAV on the resource", t1, (t1 - t0) * 1e3 / k);
  std::printf("%-34s gpu %9.3f ms  %10.2f us/barrier\n", "barrier: UAV on all (null)", t2, (t2 - t0) * 1e3 / k);
  std::fflush(stdout);
  for (auto& b : buf) b->Release();
}

static void test_rtpass() {
  const UINT w = 960, h = 540;
  const D3D12_RESOURCE_FLAGS f = D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET;
  ID3D12Resource* rt[2] = {
    texture(DXGI_FORMAT_R8G8B8A8_UNORM, w, h, f, D3D12_RESOURCE_STATE_RENDER_TARGET, true),
    texture(DXGI_FORMAT_R8G8B8A8_UNORM, w, h, f, D3D12_RESOURCE_STATE_RENDER_TARGET, true),
  };
  ID3D12Resource* fixed = texture(DXGI_FORMAT_R8G8B8A8_UNORM, w, h, f, D3D12_RESOURCE_STATE_RENDER_TARGET, true);
  init_tex(rt[0], D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_RENDER_TARGET, true);
  init_tex(rt[1], D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE, true);
  init_tex(fixed, D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE, true);
  make_rtv(rt[0], 0);
  make_rtv(rt[1], 1);
  make_srv(rt[0], 2);
  make_srv(rt[1], 3);
  make_srv(fixed, 4);
  ID3D12PipelineState* p = gfx("ps_blit", DXGI_FORMAT_R8G8B8A8_UNORM, false);
  const int k = 256;
  PC pc = { { 1.0f / w, 1.0f / h, 0, 0 }, { 0 } };
  double t1 = time_body([&](ID3D12GraphicsCommandList* l) {
    set_target(l, 0, w, h);
    l->SetPipelineState(p);
    push(l, pc);
    l->SetGraphicsRootDescriptorTable(RP_TABLE, srv_gpu(4));
    for (int i = 0; i < k; i++) l->DrawInstanced(3, 1, 0, 0);
  });
  double t2 = time_body([&](ID3D12GraphicsCommandList* l) {
    l->SetPipelineState(p);
    for (int i = 0; i < k; i++) {
      const int d = i & 1, s = (i + 1) & 1;
      set_target(l, d, w, h);
      push(l, pc);
      l->SetGraphicsRootDescriptorTable(RP_TABLE, srv_gpu(2 + s));
      l->DrawInstanced(3, 1, 0, 0);
      D3D12_RESOURCE_BARRIER b[2] = {
        transition(rt[d], D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE),
        transition(rt[s], D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE, D3D12_RESOURCE_STATE_RENDER_TARGET),
      };
      l->ResourceBarrier(2, b);
    }
  });
  report("rtpass: 256 draws 960x540, 1 pass", t1, 0, "");
  std::printf("%-34s gpu %9.3f ms  %10.2f us/pass\n", "rtpass: 256 passes + transitions", t2, (t2 - t1) * 1e3 / k);
  std::fflush(stdout);
  p->Release();
  rt[0]->Release();
  rt[1]->Release();
  fixed->Release();
  rt_heap_off = 0;
}

static void test_rootcbv() {
  const int nd = getenv("BENCH_NDRAWS") ? std::atoi(getenv("BENCH_NDRAWS")) : 20000;
  const UINT stride = 256;
  ID3D12Resource* up = buffer((UINT64)nd * stride, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_FLAG_NONE,
                              D3D12_RESOURCE_STATE_GENERIC_READ);
  ID3D12Resource* def = buffer((UINT64)nd * stride, D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_FLAG_NONE,
                               D3D12_RESOURCE_STATE_COPY_DEST);
  float* m = nullptr;
  D3D12_RANGE none = { 0, 0 };
  CHECK(up->Map(0, &none, (void**)&m));
  for (int i = 0; i < nd; i++) {
    float* d = m + (size_t)i * stride / 4;
    d[0] = -1.0f + 2.0f * ((i * 37) % 97) / 97.0f;
    d[1] = -1.0f + 2.0f * ((i * 53) % 89) / 89.0f;
    d[2] = d[3] = 0;
    d[4] = (i & 255) / 255.0f;
    d[5] = 0.5f;
    d[6] = 0.25f;
    d[7] = 1.0f;
  }
  up->Unmap(0, nullptr);
  {
    Cmd c;
    c.begin();
    c.list->CopyBufferRegion(def, 0, up, 0, (UINT64)nd * stride);
    D3D12_RESOURCE_BARRIER b =
        transition(def, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_VERTEX_AND_CONSTANT_BUFFER);
    c.list->ResourceBarrier(1, &b);
    c.close();
    run_wait(c);
    c.release();
  }
  ID3D12Resource* rt = texture(DXGI_FORMAT_R8G8B8A8_UNORM, W, H, D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                               D3D12_RESOURCE_STATE_RENDER_TARGET, false);
  make_rtv(rt, 0);
  ID3D12PipelineState* p = gfx("ps_cb", DXGI_FORMAT_R8G8B8A8_UNORM, false, "vs_quad");
  auto run = [&](ID3D12Resource* cb, bool per_draw) {
    return time_body([&](ID3D12GraphicsCommandList* l) {
      set_target(l, 0, W, H);
      l->IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
      l->SetPipelineState(p);
      const D3D12_GPU_VIRTUAL_ADDRESS va = cb->GetGPUVirtualAddress();
      for (int i = 0; i < nd; i++) {
        if (per_draw || i == 0) l->SetGraphicsRootConstantBufferView(RP_CBV, va + (UINT64)(per_draw ? i : 0) * stride);
        l->DrawInstanced(4, 1, 0, 0);
      }
    });
  };
  report("rootcbv: plain draws", run(def, false), nd / 1e6, "Mdraws/s");
  report("rootcbv: new CBV per draw, upload", run(up, true), nd / 1e6, "Mdraws/s");
  report("rootcbv: new CBV per draw, default", run(def, true), nd / 1e6, "Mdraws/s");
  p->Release();
  rt->Release();
  up->Release();
  def->Release();
}

// spin iterations for one dispatch of `groups` groups to take ~us
static UINT calibrate_spin(ID3D12Resource* out, UINT groups, double us) {
  UINT iters = 256;
  for (int round = 0; round < 4; round++) {
    double t = time_body([&](ID3D12GraphicsCommandList* l) {
      l->SetPipelineState(p_spin);
      PC pc = { { 0 }, { iters, 0, 0, 0 } };
      push(l, pc, true);
      l->SetComputeRootUnorderedAccessView(RP_UAV0, out->GetGPUVirtualAddress());
      l->Dispatch(groups, 1, 1);
    }) * 1e3;
    if (t <= 0) t = 1;
    double next = iters * us / t;
    iters = (UINT)std::min(std::max(next, 16.0), (double)(1u << 24));
  }
  return iters;
}

static void test_submit() {
  ID3D12Resource* out = buffer(1 << 20, D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                               D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
  const UINT groups = 2048;
  const double unit_us = getenv("BENCH_UNIT_US") ? std::atof(getenv("BENCH_UNIT_US")) : 50.0;
  const UINT iters = calibrate_spin(out, groups, unit_us);
  const int units = 64, inflight = 2;
  static const int splits[] = { 1, 4, 16, 64 };
  std::printf("# submit: %d units/frame, unit ~%.0f us (%u iterations), %d frames in flight\n", units, unit_us,
              iters, inflight);
  for (int nsub : splits) {
    const int per = units / nsub;
    // lists [slot][submit]; queries (slot * 64 + s) * 2
    std::vector<Cmd> cmds(inflight * nsub);
    for (int slot = 0; slot < inflight; slot++) {
      for (int s = 0; s < nsub; s++) {
        Cmd& c = cmds[slot * nsub + s];
        c.begin();
        const UINT q = (slot * 64 + s) * 2;
        ts(c.list, q);
        c.list->SetPipelineState(p_spin);
        PC pc = { { 0 }, { iters, 0, 0, 0 } };
        push(c.list, pc, true);
        c.list->SetComputeRootUnorderedAccessView(RP_UAV0, out->GetGPUVirtualAddress());
        for (int k = 0; k < per; k++) c.list->Dispatch(groups, 1, 1);
        ts(c.list, q + 1);
        // the frame's last list resolves the frame's timestamps
        if (s == nsub - 1) resolve(c.list, slot * 128, nsub * 2);
        c.close();
      }
    }
    UINT64 frame_end[2] = { 0, 0 };
    double busy_sum = 0, gap_sum = 0, t_measure = 0;
    int gap_n = 0, frames = 0, measured = 0;
    UINT64 prev_end = 0;
    const double t_start = now_ms();
    for (;;) {
      const int slot = frames % inflight;
      if (frame_end[slot]) {
        wait_value(frame_end[slot]);
        const UINT64* t = qmap + slot * 128;
        if (frames >= 2 * inflight + 4) {
          if (measured == 0) t_measure = now_ms();
          for (int s = 0; s < nsub; s++) {
            busy_sum += ts_ms(t[2 * s], t[2 * s + 1]);
            if (prev_end) {
              if (t[2 * s] > prev_end) gap_sum += ts_ms(prev_end, t[2 * s]);
              gap_n++;
            }
            prev_end = t[2 * s + 1];
          }
          measured++;
        } else {
          prev_end = t[2 * nsub - 1];
        }
      }
      if (now_ms() - t_start > 2000 && measured > 8) break;
      for (int s = 0; s < nsub; s++) execute(cmds[slot * nsub + s].list);
      frame_end[slot] = signal();
      frames++;
    }
    const double wall = now_ms() - t_measure;
    wait_value(fence_value);
    const double wall_per = wall / std::max(measured - 1, 1);
    const double busy_per = busy_sum / measured;
    char name[64];
    std::snprintf(name, sizeof(name), "submit %2d/frame", nsub);
    std::printf("%-34s wall %8.3f ms/frame  busy %8.3f ms  idle %5.1f%%  gap %7.1f us/submit\n", name, wall_per,
                busy_per, 100.0 * (1.0 - busy_per / wall_per), gap_n ? gap_sum * 1e3 / gap_n : 0.0);
    std::fflush(stdout);
    for (auto& c : cmds) c.release();
  }
  out->Release();
}

static void test_sustain() {
  ID3D12Resource* rt = texture(DXGI_FORMAT_R8G8B8A8_UNORM, W, H, D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                               D3D12_RESOURCE_STATE_RENDER_TARGET, false);
  make_rtv(rt, 0);
  ID3D12PipelineState* p = gfx("ps_alu", DXGI_FORMAT_R8G8B8A8_UNORM, false);
  UINT iters = 16;
  for (int round = 0; round < 3; round++) {
    PC pc = { { 0.5f, 0, 0, 0 }, { iters, 0, 0, 0 } };
    double t = run_fs(p, 0, -1, 1, pc, false);
    iters = (UINT)std::min(std::max(iters * 4.0 / std::max(t, 0.01), 1.0), (double)(1 << 20));
  }
  enum { INFLIGHT = 3 };
  Cmd cmds[INFLIGHT];
  for (int slot = 0; slot < INFLIGHT; slot++) {
    Cmd& c = cmds[slot];
    c.begin();
    ts(c.list, slot * 2);
    set_target(c.list, 0, W, H);
    c.list->SetPipelineState(p);
    PC pc = { { 0.5f, 0, 0, 0 }, { iters, 0, 0, 0 } };
    push(c.list, pc);
    c.list->DrawInstanced(3, 1, 0, 0);
    ts(c.list, slot * 2 + 1);
    resolve(c.list, slot * 2, 2);
    c.close();
  }
  std::vector<double> gpu;
  UINT64 first_begin = 0, last_end = 0, slot_value[INFLIGHT] = {};
  int submitted = 0;
  size_t sec_start = 0;
  const double t0 = now_ms();
  double sec_t = t0;
  std::printf("# sustain: %u iterations per batch, %d in flight, %.0f s\n", iters, (int)INFLIGHT, sustain_s);
  for (;;) {
    const int slot = submitted % INFLIGHT;
    if (slot_value[slot]) {
      wait_value(slot_value[slot]);
      const UINT64* t = qmap + slot * 2;
      if (!first_begin) first_begin = t[0];
      last_end = t[1];
      gpu.push_back(ts_ms(t[0], t[1]));
    }
    const double t = now_ms();
    if (t - sec_t >= 1000.0 && gpu.size() > sec_start) {
      std::vector<double> w(gpu.begin() + sec_start, gpu.end());
      std::printf("  t=%5.1fs  %4d submits  gpu min %.3f  p50 %.3f  max %.3f ms\n", (t - t0) / 1e3, (int)w.size(),
                  pct(w, 0.0), pct(w, 0.5), pct(w, 1.0));
      std::fflush(stdout);
      sec_start = gpu.size();
      sec_t = t;
    }
    if (t - t0 >= sustain_s * 1000.0) break;
    execute(cmds[slot].list);
    slot_value[slot] = signal();
    submitted++;
  }
  wait_value(fence_value);
  double sum = 0;
  for (double g : gpu) sum += g;
  const double span = ts_ms(first_begin, last_end);
  const double mn = pct(gpu, 0.0), p50 = pct(gpu, 0.5);
  std::printf("%-34s gpu min %.3f  p50 %.3f  p99 %.3f  max %.3f ms; p50/min %.3f; busy %.1f%% of the GPU span\n",
              "sustain alu 4k", mn, p50, pct(gpu, 0.99), pct(gpu, 1.0), p50 / mn,
              span > 0 ? 100.0 * sum / span : 0.0);
  std::fflush(stdout);
  for (auto& c : cmds) c.release();
  p->Release();
  rt->Release();
}


// CPU side of recording: what an engine's render threads pay per dispatch
// and per command list, the way vkd3d-proton sees it (allocator reset, list
// reset, root constants + root UAV + Dispatch, Close, ExecuteCommandLists).
// Per frame 8 lists x 64 tiny dispatches, 2 frames in flight; once with
// allocators reused frame after frame, once with new allocators and lists
// every frame (engines that create them per frame).
static void test_record() {
  ID3D12Resource* out = buffer(1 << 20, D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                               D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
  const int lists = 8, dispatches = 64, inflight = 2;
  for (int fresh = 0; fresh <= 1; fresh++) {
    std::vector<Cmd> cmds(inflight * lists);
    UINT64 slot_value[2] = { 0, 0 };
    double rec_ms = 0, t_measure = 0;
    int frames = 0, measured = 0;
    const double t0 = now_ms();
    for (;;) {
      const int slot = frames % inflight;
      if (slot_value[slot]) wait_value(slot_value[slot]);
      if (frames == 8) t_measure = now_ms();
      if (now_ms() - t0 > 2500 && measured > 8) break;
      const double r0 = now_ms();
      for (int k = 0; k < lists; k++) {
        Cmd& c = cmds[slot * lists + k];
        if (fresh) c.release();
        c.begin();
        c.list->SetPipelineState(p_copy);
        c.list->SetComputeRootUnorderedAccessView(RP_UAV0, out->GetGPUVirtualAddress());
        c.list->SetComputeRootUnorderedAccessView(RP_UAV1, out->GetGPUVirtualAddress() + (512 << 10));
        for (int d = 0; d < dispatches; d++) {
          PC pc = { { (float)d, 0, 0, 0 }, { 256, (UINT)d, 0, 0 } };
          push(c.list, pc, true);
          c.list->Dispatch(1, 1, 1);
        }
        c.close();
        execute(c.list);
      }
      const double r1 = now_ms();
      slot_value[slot] = signal();
      if (frames >= 8) {
        rec_ms += r1 - r0;
        measured++;
      }
      frames++;
    }
    const double wall = now_ms() - t_measure;
    wait_value(fence_value);
    for (auto& c : cmds) c.release();
    char name[64];
    std::snprintf(name, sizeof(name), "record %s", fresh ? "new allocators" : "reused allocators");
    std::printf("%-34s wall %8.3f ms/frame  record+submit %8.3f ms/frame  %6.2f us/dispatch\n", name,
                wall / measured, rec_ms / measured, rec_ms * 1e3 / measured / (lists * dispatches));
    std::fflush(stdout);
  }
  out->Release();
}

static bool want(const char* tests, const char* t) {
  if (!tests) return true;
  size_t n = std::strlen(t);
  for (const char* p = tests; (p = std::strstr(p, t)) != nullptr; p += n)
    if ((p == tests || p[-1] == ',') && (p[n] == ',' || p[n] == '\0')) return true;
  return false;
}

int main(int argc, char** argv) {
  const char* tests = nullptr;
  for (int i = 1; i < argc; i++) {
    if (!std::strcmp(argv[i], "-r") && i + 1 < argc) reps = std::atoi(argv[++i]);
    else if (!std::strcmp(argv[i], "-t") && i + 1 < argc) tests = argv[++i];
    else if (!std::strcmp(argv[i], "-w") && i + 1 < argc) W = std::atoi(argv[++i]);
    else if (!std::strcmp(argv[i], "-h") && i + 1 < argc) H = std::atoi(argv[++i]);
    else if (!std::strcmp(argv[i], "-s") && i + 1 < argc) sustain_s = std::atof(argv[++i]);
    else {
      std::printf("usage: %s [-r reps] [-t test,...] [-w W -h H] [-s seconds]\n", argv[0]);
      return 2;
    }
  }
  reps = std::min(std::max(reps, 1), 64);
  setvbuf(stdout, nullptr, _IONBF, 0);

  HMODULE d3d12 = LoadLibraryA("d3d12.dll");
  if (!d3d12) {
    std::printf("FAIL: d3d12.dll did not load (%lu)\n", GetLastError());
    return 2;
  }
  auto create_device = (PfnCreateDevice)(void*)GetProcAddress(d3d12, "D3D12CreateDevice");
  auto serialize_rs = (PfnSerializeRs)(void*)GetProcAddress(d3d12, "D3D12SerializeRootSignature");
  if (!create_device || !serialize_rs) {
    std::printf("FAIL: d3d12.dll lacks exports\n");
    return 2;
  }
  IDXGIFactory4* factory = nullptr;
  CHECK(CreateDXGIFactory2(0, __uuidof(IDXGIFactory4), (void**)&factory));
  IDXGIAdapter1* adapter = nullptr;
  const UINT ai = getenv("BENCH_ADAPTER") ? std::atoi(getenv("BENCH_ADAPTER")) : 0;
  CHECK(factory->EnumAdapters1(ai, &adapter));
  DXGI_ADAPTER_DESC1 ad = {};
  adapter->GetDesc1(&ad);
  CHECK(create_device(adapter, D3D_FEATURE_LEVEL_11_0, __uuidof(ID3D12Device), (void**)&dev));

  D3D12_COMMAND_QUEUE_DESC qd = {};
  qd.Type = D3D12_COMMAND_LIST_TYPE_DIRECT;
  CHECK(dev->CreateCommandQueue(&qd, __uuidof(ID3D12CommandQueue), (void**)&queue));
  CHECK(dev->CreateFence(0, D3D12_FENCE_FLAG_NONE, __uuidof(ID3D12Fence), (void**)&fence));
  fence_event = CreateEventA(nullptr, FALSE, FALSE, nullptr);
  UINT64 freq = 0;
  CHECK(queue->GetTimestampFrequency(&freq));
  ts_period_ms = 1e3 / (double)freq;
  std::printf("# d3d12_gpu_bound: %ls, %ux%u, median of %d, timestamp %llu Hz\n", ad.Description, W, H, reps,
              (unsigned long long)freq);

  // Root signature: 8 root constants b0, a one-SRV table t0, root UAVs u0
  // and u1, a root CBV b1, a static linear wrap sampler s0.
  D3D12_DESCRIPTOR_RANGE range = {};
  range.RangeType = D3D12_DESCRIPTOR_RANGE_TYPE_SRV;
  range.NumDescriptors = 1;
  D3D12_ROOT_PARAMETER rp[5] = {};
  rp[RP_CONST].ParameterType = D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS;
  rp[RP_CONST].Constants.Num32BitValues = 8;
  rp[RP_TABLE].ParameterType = D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE;
  rp[RP_TABLE].DescriptorTable.NumDescriptorRanges = 1;
  rp[RP_TABLE].DescriptorTable.pDescriptorRanges = &range;
  rp[RP_UAV0].ParameterType = D3D12_ROOT_PARAMETER_TYPE_UAV;
  rp[RP_UAV0].Descriptor.ShaderRegister = 0;
  rp[RP_UAV1].ParameterType = D3D12_ROOT_PARAMETER_TYPE_UAV;
  rp[RP_UAV1].Descriptor.ShaderRegister = 1;
  rp[RP_CBV].ParameterType = D3D12_ROOT_PARAMETER_TYPE_CBV;
  rp[RP_CBV].Descriptor.ShaderRegister = 1;
  for (auto& r : rp) r.ShaderVisibility = D3D12_SHADER_VISIBILITY_ALL;
  D3D12_STATIC_SAMPLER_DESC ss = {};
  ss.Filter = D3D12_FILTER_MIN_MAG_MIP_LINEAR;
  ss.AddressU = ss.AddressV = ss.AddressW = D3D12_TEXTURE_ADDRESS_MODE_WRAP;
  ss.MaxLOD = D3D12_FLOAT32_MAX;
  ss.ShaderVisibility = D3D12_SHADER_VISIBILITY_ALL;
  D3D12_ROOT_SIGNATURE_DESC rsd = {};
  rsd.NumParameters = 5;
  rsd.pParameters = rp;
  rsd.NumStaticSamplers = 1;
  rsd.pStaticSamplers = &ss;
  ID3DBlob* rsb = nullptr;
  ID3DBlob* rse = nullptr;
  if (FAILED(serialize_rs(&rsd, D3D_ROOT_SIGNATURE_VERSION_1, &rsb, &rse))) {
    std::printf("FAIL: root signature: %s\n", rse ? (const char*)rse->GetBufferPointer() : "?");
    return 2;
  }
  CHECK(dev->CreateRootSignature(0, rsb->GetBufferPointer(), rsb->GetBufferSize(), __uuidof(ID3D12RootSignature),
                                 (void**)&rs));

  D3D12_QUERY_HEAP_DESC qhd = {};
  qhd.Type = D3D12_QUERY_HEAP_TYPE_TIMESTAMP;
  qhd.Count = NQUERIES;
  CHECK(dev->CreateQueryHeap(&qhd, __uuidof(ID3D12QueryHeap), (void**)&qheap));
  qreadback = buffer(NQUERIES * 8, D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_FLAG_NONE,
                     D3D12_RESOURCE_STATE_COPY_DEST);
  CHECK(qreadback->Map(0, nullptr, (void**)&qmap));  // persistently mapped readback

  D3D12_DESCRIPTOR_HEAP_DESC dh = {};
  dh.Type = D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV;
  dh.NumDescriptors = 16;
  dh.Flags = D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE;
  CHECK(dev->CreateDescriptorHeap(&dh, __uuidof(ID3D12DescriptorHeap), (void**)&srv_heap));
  dh.Type = D3D12_DESCRIPTOR_HEAP_TYPE_RTV;
  dh.Flags = D3D12_DESCRIPTOR_HEAP_FLAG_NONE;
  CHECK(dev->CreateDescriptorHeap(&dh, __uuidof(ID3D12DescriptorHeap), (void**)&rtv_heap));
  srv_inc = dev->GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
  rtv_inc = dev->GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV);

  p_copy = comp("cs_copy");
  p_spin = comp("cs_spin");

  if (want(tests, "alu") || want(tests, "fma")) test_alu(want(tests, "alu"), want(tests, "fma"));
  if (want(tests, "fill")) test_fill();
  if (want(tests, "tex")) test_tex();
  if (want(tests, "bw")) test_bw();
  if (want(tests, "barrier")) test_barrier();
  if (want(tests, "rtpass")) test_rtpass();
  if (want(tests, "rootcbv")) test_rootcbv();
  if (want(tests, "record")) test_record();
  if (want(tests, "submit")) test_submit();
  if (want(tests, "sustain")) test_sustain();

  wait_value(fence_value);
  std::printf("# done\n");
  return 0;
}
