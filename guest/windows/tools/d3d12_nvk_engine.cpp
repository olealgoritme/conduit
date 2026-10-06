// d3d12_nvk_engine: the Helios D3D12 engine (vkd3d-proton fork) on NVK on RM,
// through the same entry points helios_umd12.dll uses, without the UMD or the
// D3D12 runtime (dxvk-on-nvk S5 engine gate).
//
//   d3d12_nvk_engine.exe <vulkan_nouveau.dll> [helios_vkd3d.dll] [frames=200]
//
// Loads NVK directly (vk_icdNegotiateLoaderICDInterfaceVersion +
// vk_icdGetInstanceProcAddr, as umd_common/bridge/bridge_icd_backend.cpp does),
// creates the device with helios_vkd3d_create_device_icd, runs the native
// feature-level admission for 11_0 / 12_0 / 12_1, then:
//   - a swapchain-like texture on an export heap (heap flag 1 << 30, placed at
//     offset 0, as umd12's fused heap+resource path does), cleared, read back;
//   - its KMD resource id through NVK's helios_icd_interface_v2 memory_res_id
//     (needs the KMD's IMPORT_RM gate; reported, not required);
//   - N executions through helios_vkd3d_execute_command_lists, each waited with
//     helios_vkd3d_wait_execution (the NVK ECL path), with the latency of each.
// NVK_RM=1 is set by this program. Exit 0 when the clear reads back exactly.
//
// Build (MinGW-w64 on Linux, with the Vulkan headers):
//   x86_64-w64-mingw32-g++ -O2 -static d3d12_nvk_engine.cpp -I<vulkan-include>
//       -I../protocol/include -o d3d12_nvk_engine.exe -luuid
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d12.h>

#define VK_NO_PROTOTYPES
#include <vulkan/vulkan.h>

#include "helios_icd_interface.h"

#include <algorithm>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <vector>

#define CHECK(expr)                                                                   \
  do {                                                                                \
    HRESULT hr_ = (expr);                                                             \
    if (FAILED(hr_)) {                                                                \
      std::printf("FAIL %s:%d %s -> 0x%08lx\n", __FILE__, __LINE__, #expr, (unsigned long)hr_); \
      std::exit(2);                                                                   \
    }                                                                                 \
  } while (0)

// ID3D12DXVKInteropDevice4, as umd12/bridge/vkd3d_bridge.cpp declares it.
struct ID3D12DXVKInteropDevice4 : public IUnknown {
  virtual HRESULT STDMETHODCALLTYPE GetDXGIAdapter(REFIID, void**) = 0;
  virtual HRESULT STDMETHODCALLTYPE GetInstanceExtensions(UINT*, const char**) = 0;
  virtual HRESULT STDMETHODCALLTYPE GetDeviceExtensions(UINT*, const char**) = 0;
  virtual HRESULT STDMETHODCALLTYPE GetDeviceFeatures(const void**) = 0;
  virtual HRESULT STDMETHODCALLTYPE GetVulkanHandles(void**, void**, void**) = 0;
  virtual HRESULT STDMETHODCALLTYPE GetVulkanQueueInfo(ID3D12CommandQueue*, void**, UINT32*) = 0;
  virtual void STDMETHODCALLTYPE GetVulkanImageLayout(ID3D12Resource*, D3D12_RESOURCE_STATES, int*) = 0;
  virtual HRESULT STDMETHODCALLTYPE GetVulkanResourceInfo(ID3D12Resource*, UINT64*, UINT64*) = 0;
  virtual HRESULT STDMETHODCALLTYPE LockCommandQueue(ID3D12CommandQueue*) = 0;
  virtual HRESULT STDMETHODCALLTYPE UnlockCommandQueue(ID3D12CommandQueue*) = 0;
  virtual HRESULT STDMETHODCALLTYPE GetVulkanResourceInfo1(ID3D12Resource*, UINT64*, UINT64*, int*) = 0;
  virtual HRESULT STDMETHODCALLTYPE CreateInteropCommandQueue(const D3D12_COMMAND_QUEUE_DESC*, UINT32,
                                                              ID3D12CommandQueue**) = 0;
  virtual HRESULT STDMETHODCALLTYPE CreateInteropCommandAllocator(D3D12_COMMAND_LIST_TYPE, UINT32,
                                                                  ID3D12CommandAllocator**) = 0;
  virtual HRESULT STDMETHODCALLTYPE BeginVkCommandBufferInterop(ID3D12CommandList*, void**) = 0;
  virtual HRESULT STDMETHODCALLTYPE EndVkCommandBufferInterop(ID3D12CommandList*) = 0;
  virtual HRESULT STDMETHODCALLTYPE LockVulkanQueue(ID3D12CommandQueue*) = 0;
  virtual HRESULT STDMETHODCALLTYPE UnlockVulkanQueue(ID3D12CommandQueue*) = 0;
  virtual HRESULT STDMETHODCALLTYPE GetVulkanHeapInfo(ID3D12Heap*, UINT64*, UINT64*, UINT32*) = 0;
  virtual HRESULT STDMETHODCALLTYPE GetVulkanResourceMemoryInfo(ID3D12Resource*, UINT64*, UINT64*, UINT64*,
                                                                UINT32*) = 0;
};
static const GUID kIidInterop4 = {0x9c0850e7, 0x70f1, 0x4229, {0xae, 0x05, 0x44, 0x0b, 0x38, 0x7e, 0xc5, 0x17}};

typedef HRESULT(__cdecl* PfnCreateIcd)(LUID, PFN_vkGetInstanceProcAddr, REFIID, void**);
typedef HRESULT(__cdecl* PfnValidate)(ID3D12Device*, UINT32, UINT32*, UINT32*, UINT8*);
typedef HRESULT(__cdecl* PfnExecute)(ID3D12CommandQueue*, UINT, ID3D12CommandList* const*, HANDLE, UINT32*,
                                     UINT32*, UINT64*);
typedef HRESULT(__cdecl* PfnWait)(ID3D12CommandQueue*, UINT64, UINT64);
typedef VkResult(VKAPI_PTR* PfnNegotiate)(uint32_t*);

static double now_ms() {
  static LARGE_INTEGER f = {};
  if (!f.QuadPart) QueryPerformanceFrequency(&f);
  LARGE_INTEGER t;
  QueryPerformanceCounter(&t);
  return double(t.QuadPart) * 1000.0 / double(f.QuadPart);
}

int main(int argc, char** argv) {
  setvbuf(stdout, nullptr, _IONBF, 0);
  if (argc < 2) {
    std::printf("usage: d3d12_nvk_engine <vulkan_nouveau.dll> [helios_vkd3d.dll] [frames]\n");
    return 2;
  }
  const char* engine_path = argc > 2 ? argv[2] : "helios_vkd3d.dll";
  const int frames = argc > 3 ? std::atoi(argv[3]) : 200;
  SetEnvironmentVariableA("NVK_RM", "1");

  HMODULE nvk = LoadLibraryExA(argv[1], nullptr, LOAD_WITH_ALTERED_SEARCH_PATH);
  if (!nvk) { std::printf("FAIL: %s did not load (%lu)\n", argv[1], GetLastError()); return 2; }
  auto negotiate = (PfnNegotiate)(void*)GetProcAddress(nvk, "vk_icdNegotiateLoaderICDInterfaceVersion");
  auto gipa = (PFN_vkGetInstanceProcAddr)(void*)GetProcAddress(nvk, "vk_icdGetInstanceProcAddr");
  auto iface = (PFN_helios_icd_interface_v2)(void*)GetProcAddress(nvk, HELIOS_ICD_INTERFACE_EXPORT);
  if (!negotiate || !gipa) { std::printf("FAIL: no vk_icd entry points\n"); return 2; }
  uint32_t loader_version = 7;
  if (negotiate(&loader_version) != VK_SUCCESS) { std::printf("FAIL: negotiation\n"); return 2; }
  helios_icd_api api = {};
  api.size = sizeof(api);
  const bool have_api = iface && iface(HELIOS_ICD_INTERFACE_VERSION, &api) == VK_SUCCESS;
  std::printf("nvk: loader interface %u, helios_icd_interface_v2 %s (backend %u caps 0x%x)\n", loader_version,
              have_api ? "yes" : "NO", api.backend, api.caps);

  HMODULE engine = LoadLibraryA(engine_path);
  if (!engine) { std::printf("FAIL: %s did not load (%lu)\n", engine_path, GetLastError()); return 2; }
  auto create_icd = (PfnCreateIcd)(void*)GetProcAddress(engine, "helios_vkd3d_create_device_icd");
  auto validate = (PfnValidate)(void*)GetProcAddress(engine, "helios_vkd3d_validate_native_feature_level");
  auto execute = (PfnExecute)(void*)GetProcAddress(engine, "helios_vkd3d_execute_command_lists");
  auto wait = (PfnWait)(void*)GetProcAddress(engine, "helios_vkd3d_wait_execution");
  if (!create_icd || !validate || !execute || !wait) { std::printf("FAIL: engine lacks S5 exports\n"); return 2; }

  ID3D12Device* dev = nullptr;
  const double t0 = now_ms();
  CHECK(create_icd(LUID{0, 0}, gipa, __uuidof(ID3D12Device), (void**)&dev));
  std::printf("helios_vkd3d_create_device_icd OK in %.1f ms\n", now_ms() - t0);

  const UINT32 levels[] = {0xb000, 0xc000, 0xc100};
  for (UINT32 level : levels) {
    UINT32 sm = 0, rt = 0;
    UINT8 uuid[16] = {};
    const HRESULT hr = validate(dev, level, &sm, &rt, uuid);
    std::printf("native admission FL 0x%x: hr=0x%08lx SM 0x%x RT %u\n", level, (unsigned long)hr, sm, rt);
  }
  D3D12_FEATURE_DATA_D3D12_OPTIONS o = {};
  dev->CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS, &o, sizeof(o));
  std::printf("engine OPTIONS: ROVs %d conservative raster %d binding %d tiled %d\n", o.ROVsSupported,
              o.ConservativeRasterizationTier, o.ResourceBindingTier, o.TiledResourcesTier);

  ID3D12DXVKInteropDevice4* interop = nullptr;
  CHECK(dev->QueryInterface(kIidInterop4, (void**)&interop));
  void *vk_instance = nullptr, *vk_phys = nullptr, *vk_device = nullptr;
  CHECK(interop->GetVulkanHandles(&vk_instance, &vk_phys, &vk_device));

  D3D12_COMMAND_QUEUE_DESC qd = {};
  qd.Type = D3D12_COMMAND_LIST_TYPE_DIRECT;
  ID3D12CommandQueue* queue = nullptr;
  CHECK(dev->CreateCommandQueue(&qd, __uuidof(ID3D12CommandQueue), (void**)&queue));
  ID3D12CommandAllocator* alloc = nullptr;
  CHECK(dev->CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT, __uuidof(ID3D12CommandAllocator), (void**)&alloc));
  ID3D12GraphicsCommandList* list = nullptr;
  CHECK(dev->CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, alloc, nullptr, __uuidof(ID3D12GraphicsCommandList),
                               (void**)&list));
  CHECK(list->Close());

  // A back buffer the way umd12 makes one: export heap + texture placed at 0.
  const UINT w = 1280, h = 720;
  D3D12_RESOURCE_DESC td = {};
  td.Dimension = D3D12_RESOURCE_DIMENSION_TEXTURE2D;
  td.Width = w;
  td.Height = h;
  td.DepthOrArraySize = 1;
  td.MipLevels = 1;
  td.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
  td.SampleDesc.Count = 1;
  td.Flags = D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET;
  D3D12_RESOURCE_ALLOCATION_INFO ai = dev->GetResourceAllocationInfo(0, 1, &td);
  D3D12_HEAP_DESC hd = {};
  hd.SizeInBytes = ai.SizeInBytes;
  hd.Alignment = ai.Alignment;
  hd.Properties.Type = D3D12_HEAP_TYPE_CUSTOM;
  hd.Properties.CPUPageProperty = D3D12_CPU_PAGE_PROPERTY_NOT_AVAILABLE;
  hd.Properties.MemoryPoolPreference = D3D12_MEMORY_POOL_L1;
  hd.Flags = (D3D12_HEAP_FLAGS)(D3D12_HEAP_FLAG_ALLOW_ONLY_RT_DS_TEXTURES | (1u << 30));
  ID3D12Heap* heap = nullptr;
  CHECK(dev->CreateHeap(&hd, __uuidof(ID3D12Heap), (void**)&heap));
  ID3D12Resource* tex = nullptr;
  CHECK(dev->CreatePlacedResource(heap, 0, &td, D3D12_RESOURCE_STATE_RENDER_TARGET, nullptr,
                                  __uuidof(ID3D12Resource), (void**)&tex));
  UINT64 vk_memory = 0, mem_offset = 0, mem_size = 0, vk_image = 0, buf_off = 0;
  UINT32 mem_type = 0;
  CHECK(interop->GetVulkanResourceMemoryInfo(tex, &vk_memory, &mem_offset, &mem_size, &mem_type));
  CHECK(interop->GetVulkanResourceInfo(tex, &vk_image, &buf_off));
  std::printf("export texture: vk_memory 0x%llx offset %llu size %llu type %u image 0x%llx\n",
              (unsigned long long)vk_memory, (unsigned long long)mem_offset, (unsigned long long)mem_size, mem_type,
              (unsigned long long)vk_image);
  if (have_api && api.memory_res_id) {
    uint32_t res_id = 0;
    helios_icd_layout layout = {};
    const VkResult vr = api.memory_res_id((VkDevice)vk_device, (VkDeviceMemory)vk_memory, (VkImage)vk_image, &res_id,
                                          &layout);
    std::printf("memory_res_id: vr=%d res_id=%u %ux%u stride %u modifier 0x%016llx size %llu (ctx %u)\n", int(vr),
                res_id, layout.width, layout.height, layout.stride, (unsigned long long)layout.modifier,
                (unsigned long long)layout.size, api.ctx_id ? api.ctx_id((VkInstance)vk_instance) : 0);
  }

  D3D12_DESCRIPTOR_HEAP_DESC dh = {};
  dh.Type = D3D12_DESCRIPTOR_HEAP_TYPE_RTV;
  dh.NumDescriptors = 1;
  ID3D12DescriptorHeap* rtv_heap = nullptr;
  CHECK(dev->CreateDescriptorHeap(&dh, __uuidof(ID3D12DescriptorHeap), (void**)&rtv_heap));
  D3D12_CPU_DESCRIPTOR_HANDLE rtv = rtv_heap->GetCPUDescriptorHandleForHeapStart();
  dev->CreateRenderTargetView(tex, nullptr, rtv);

  D3D12_HEAP_PROPERTIES rbp = {};
  rbp.Type = D3D12_HEAP_TYPE_READBACK;
  D3D12_RESOURCE_DESC bd = {};
  bd.Dimension = D3D12_RESOURCE_DIMENSION_BUFFER;
  bd.Width = UINT64(w) * 4 * h;
  bd.Height = 1;
  bd.DepthOrArraySize = 1;
  bd.MipLevels = 1;
  bd.SampleDesc.Count = 1;
  bd.Layout = D3D12_TEXTURE_LAYOUT_ROW_MAJOR;
  ID3D12Resource* rb = nullptr;
  CHECK(dev->CreateCommittedResource(&rbp, D3D12_HEAP_FLAG_NONE, &bd, D3D12_RESOURCE_STATE_COPY_DEST, nullptr,
                                     __uuidof(ID3D12Resource), (void**)&rb));

  HANDLE admitted = CreateEventA(nullptr, TRUE, TRUE, nullptr);  // admission already granted
  std::vector<double> lat;
  UINT32 last_value = 0;
  bool ok = true;
  for (int f = 0; f < frames; f++) {
    CHECK(alloc->Reset());
    CHECK(list->Reset(alloc, nullptr));
    const float c = float(f % 256) / 255.0f;
    const float clear[4] = {c, 0.25f, 0.5f, 1.0f};
    list->ClearRenderTargetView(rtv, clear, 0, nullptr);
    const bool last = f == frames - 1;
    if (last) {
      D3D12_RESOURCE_BARRIER b = {};
      b.Type = D3D12_RESOURCE_BARRIER_TYPE_TRANSITION;
      b.Transition.pResource = tex;
      b.Transition.StateBefore = D3D12_RESOURCE_STATE_RENDER_TARGET;
      b.Transition.StateAfter = D3D12_RESOURCE_STATE_COPY_SOURCE;
      list->ResourceBarrier(1, &b);
      D3D12_TEXTURE_COPY_LOCATION dst = {}, src = {};
      dst.pResource = rb;
      dst.Type = D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT;
      dst.PlacedFootprint.Footprint = {DXGI_FORMAT_B8G8R8A8_UNORM, w, h, 1, w * 4};
      src.pResource = tex;
      src.Type = D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX;
      list->CopyTextureRegion(&dst, 0, 0, 0, &src, nullptr);
    }
    CHECK(list->Close());
    ID3D12CommandList* lists[] = {list};
    UINT32 ctx = 0, value = 0;
    UINT64 cookie = 0;
    const double s = now_ms();
    CHECK(execute(queue, 1, lists, admitted, &ctx, &value, &cookie));
    if (value <= last_value || ctx != 0 || cookie != 0) {
      std::printf("FAIL: execution boundary ctx=%u value=%u cookie=%llu (previous value %u)\n", ctx, value,
                  (unsigned long long)cookie, last_value);
      return 3;
    }
    last_value = value;
    const HRESULT whr = wait(queue, value, 2000000000ull);
    if (whr != S_OK) {
      std::printf("FAIL: helios_vkd3d_wait_execution(%u) -> 0x%08lx\n", value, (unsigned long)whr);
      return 3;
    }
    lat.push_back(now_ms() - s);
    if (last) {
      UINT8* p = nullptr;
      D3D12_RANGE r = {0, SIZE_T(bd.Width)};
      CHECK(rb->Map(0, &r, (void**)&p));
      const UINT32 px = ((UINT32*)p)[(h / 2) * w + w / 2];
      rb->Unmap(0, nullptr);
      // BGRA8 little endian: 0xAARRGGBB. Clear (c, 0.25, 0.5, 1).
      const UINT32 want = 0xff000000u | (UINT32(c * 255.0f + 0.5f) << 16) | (64u << 8) | 128u;
      const auto close_to = [](UINT32 a, UINT32 b) {
        for (int k = 0; k < 32; k += 8)
          if (std::abs(int((a >> k) & 0xff) - int((b >> k) & 0xff)) > 1) return false;
        return true;
      };
      ok = close_to(px, want);
      std::printf("readback: 0x%08x want ~0x%08x -> %s\n", px, want, ok ? "OK" : "FAIL");
    }
  }
  std::sort(lat.begin(), lat.end());
  std::printf("executions %d (last value %u): ECL->complete median %.3f ms p95 %.3f ms max %.3f ms\n", frames,
              last_value, lat[lat.size() / 2], lat[lat.size() * 95 / 100], lat.back());
  std::printf("result: %s\n", ok ? "PASS" : "FAIL");
  return ok ? 0 : 1;
}
