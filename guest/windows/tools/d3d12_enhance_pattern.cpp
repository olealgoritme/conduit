// d3d12_enhance_pattern: a D3D12 game stand-in for upscaling and frame
// generation (guest/nvk-rm/docs/enhance.md): a borderless window covering
// the primary monitor whose flip-model swap chain is smaller (the game's
// render resolution), drawn by clears only (no shaders): a grid, a white
// box moving at a fixed speed, a static "HUD" bar, at a capped frame rate.
//
//   d3d12_enhance_pattern.exe [seconds=10] [width=1280 height=720] [fps cap=60]
//                             [resize WxH]
//
// With a resize size, the swap chain is resized to it half way through
// (ResizeBuffers), as a game does when its resolution changes.
//
// Prints the frame rate and the time Present took (average and worst).
//
// Build (MinGW-w64 on Linux):
//   x86_64-w64-mingw32-g++ -O2 -static d3d12_enhance_pattern.cpp -o d3d12_enhance_pattern.exe \
//       -ld3d12 -ldxgi -lgdi32 -luser32 -lwinmm
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <mmsystem.h>
#include <d3d12.h>
#include <dxgi1_4.h>

#include <cmath>
#include <cstdio>
#include <cstdlib>

static LRESULT CALLBACK wndproc(HWND h, UINT m, WPARAM w, LPARAM l) {
  if (m == WM_DESTROY) { PostQuitMessage(0); return 0; }
  return DefWindowProcA(h, m, w, l);
}

#define CHECK(x) do { HRESULT hr_ = (x); if (FAILED(hr_)) { \
  std::printf("%s: 0x%08lx\n", #x, (unsigned long)hr_); return 1; } } while (0)

int main(int argc, char** argv) {
  const int seconds = argc > 1 ? std::atoi(argv[1]) : 10;
  UINT width = argc > 3 ? std::atoi(argv[2]) : 1280;
  UINT height = argc > 3 ? std::atoi(argv[3]) : 720;
  const double cap = argc > 4 ? std::atof(argv[4]) : 60.0;
  unsigned rw = 0, rh = 0;
  if (argc > 5) std::sscanf(argv[5], "%ux%u", &rw, &rh);
  const UINT N = 3;

  timeBeginPeriod(1);
  SetProcessDPIAware();
  WNDCLASSA wc = {};
  wc.lpfnWndProc = wndproc;
  wc.hInstance = GetModuleHandleA(nullptr);
  wc.lpszClassName = "d3d12_enhance_pattern";
  RegisterClassA(&wc);
  const int sw = GetSystemMetrics(SM_CXSCREEN), sh = GetSystemMetrics(SM_CYSCREEN);
  HWND hwnd = CreateWindowA("d3d12_enhance_pattern", "d3d12_enhance_pattern",
                            WS_POPUP | WS_VISIBLE, 0, 0, sw, sh, nullptr, nullptr,
                            wc.hInstance, nullptr);
  SetForegroundWindow(hwnd);

  IDXGIFactory4* factory = nullptr;
  CHECK(CreateDXGIFactory1(__uuidof(IDXGIFactory4), (void**)&factory));
  ID3D12Device* dev = nullptr;
  CHECK(D3D12CreateDevice(nullptr, D3D_FEATURE_LEVEL_11_0, __uuidof(ID3D12Device), (void**)&dev));
  D3D12_COMMAND_QUEUE_DESC qd = {};
  qd.Type = D3D12_COMMAND_LIST_TYPE_DIRECT;
  ID3D12CommandQueue* queue = nullptr;
  CHECK(dev->CreateCommandQueue(&qd, __uuidof(ID3D12CommandQueue), (void**)&queue));

  DXGI_SWAP_CHAIN_DESC1 sd = {};
  sd.Width = width;
  sd.Height = height;
  sd.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
  sd.SampleDesc.Count = 1;
  sd.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
  sd.BufferCount = N;
  sd.Scaling = DXGI_SCALING_STRETCH;
  sd.SwapEffect = DXGI_SWAP_EFFECT_FLIP_DISCARD;
  IDXGISwapChain1* sc1 = nullptr;
  CHECK(factory->CreateSwapChainForHwnd(queue, hwnd, &sd, nullptr, nullptr, &sc1));
  IDXGISwapChain3* sc = nullptr;
  CHECK(sc1->QueryInterface(__uuidof(IDXGISwapChain3), (void**)&sc));
  std::printf("window %dx%d, D3D12 swap chain %ux%u x%u\n", sw, sh, width, height, N);

  D3D12_DESCRIPTOR_HEAP_DESC hd = {};
  hd.Type = D3D12_DESCRIPTOR_HEAP_TYPE_RTV;
  hd.NumDescriptors = N;
  ID3D12DescriptorHeap* heap = nullptr;
  CHECK(dev->CreateDescriptorHeap(&hd, __uuidof(ID3D12DescriptorHeap), (void**)&heap));
  const UINT inc = dev->GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV);
  D3D12_CPU_DESCRIPTOR_HANDLE base = heap->GetCPUDescriptorHandleForHeapStart();
  ID3D12Resource* bufs[N];
  ID3D12CommandAllocator* alloc[N];
  for (UINT i = 0; i < N; i++) {
    CHECK(sc->GetBuffer(i, __uuidof(ID3D12Resource), (void**)&bufs[i]));
    D3D12_CPU_DESCRIPTOR_HANDLE h = base;
    h.ptr += i * inc;
    dev->CreateRenderTargetView(bufs[i], nullptr, h);
    CHECK(dev->CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT,
                                      __uuidof(ID3D12CommandAllocator), (void**)&alloc[i]));
  }
  ID3D12GraphicsCommandList* list = nullptr;
  CHECK(dev->CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, alloc[0], nullptr,
                               __uuidof(ID3D12GraphicsCommandList), (void**)&list));
  list->Close();
  ID3D12Fence* fence = nullptr;
  CHECK(dev->CreateFence(0, D3D12_FENCE_FLAG_NONE, __uuidof(ID3D12Fence), (void**)&fence));
  HANDLE ev = CreateEventA(nullptr, FALSE, FALSE, nullptr);
  UINT64 fv = 0, done_at[N] = {};

  LARGE_INTEGER freq, start;
  QueryPerformanceFrequency(&freq);
  QueryPerformanceCounter(&start);
  auto secs = [&]() { LARGE_INTEGER t; QueryPerformanceCounter(&t);
                      return double(t.QuadPart - start.QuadPart) / freq.QuadPart; };
  unsigned frames = 0, failed = 0;
  double next = 0, psum = 0, pmax = 0;
  MSG msg;
  bool quit = false;
  while (!quit) {
    while (PeekMessageA(&msg, nullptr, 0, 0, PM_REMOVE)) {
      if (msg.message == WM_QUIT) quit = true;
      TranslateMessage(&msg);
      DispatchMessageA(&msg);
    }
    double t = secs();
    if (t >= seconds) break;
    if (cap > 0) {
      if (t < next) { Sleep(DWORD((next - t) * 1000.0)); while (secs() < next) {} }
      next += 1.0 / cap;
      if (secs() > next + 0.1) next = secs();
      t = secs();
    }
    if (rw && t >= seconds / 2.0) {
      queue->Signal(fence, ++fv);
      fence->SetEventOnCompletion(fv, ev);
      WaitForSingleObject(ev, 2000);
      for (UINT b = 0; b < N; b++) bufs[b]->Release();
      HRESULT hr = sc->ResizeBuffers(0, rw, rh, DXGI_FORMAT_UNKNOWN, 0);
      std::printf("ResizeBuffers(%ux%u): 0x%08lx\n", rw, rh, (unsigned long)hr);
      for (UINT b = 0; b < N; b++) {
        CHECK(sc->GetBuffer(b, __uuidof(ID3D12Resource), (void**)&bufs[b]));
        D3D12_CPU_DESCRIPTOR_HANDLE h = base;
        h.ptr += b * inc;
        dev->CreateRenderTargetView(bufs[b], nullptr, h);
        done_at[b] = 0;
      }
      width = rw;
      height = rh;
      rw = 0;
    }
    const UINT i = sc->GetCurrentBackBufferIndex();
    if (fence->GetCompletedValue() < done_at[i]) {
      fence->SetEventOnCompletion(done_at[i], ev);
      WaitForSingleObject(ev, 2000);
    }
    alloc[i]->Reset();
    list->Reset(alloc[i], nullptr);
    D3D12_RESOURCE_BARRIER b = {};
    b.Type = D3D12_RESOURCE_BARRIER_TYPE_TRANSITION;
    b.Transition.pResource = bufs[i];
    b.Transition.Subresource = D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES;
    b.Transition.StateBefore = D3D12_RESOURCE_STATE_PRESENT;
    b.Transition.StateAfter = D3D12_RESOURCE_STATE_RENDER_TARGET;
    list->ResourceBarrier(1, &b);
    D3D12_CPU_DESCRIPTOR_HANDLE rtv = base;
    rtv.ptr += i * inc;
    const float bg[4] = {0.05f, 0.08f, 0.2f, 1.0f};
    list->ClearRenderTargetView(rtv, bg, 0, nullptr);
    D3D12_RECT grid[128];
    UINT ng = 0;
    for (UINT x = 0; x < width && ng < 64; x += 32) grid[ng++] = {LONG(x), 0, LONG(x + 1), LONG(height)};
    for (UINT y = 0; y < height && ng < 128; y += 32) grid[ng++] = {0, LONG(y), LONG(width), LONG(y + 1)};
    const float grey[4] = {0.5f, 0.5f, 0.5f, 1};
    list->ClearRenderTargetView(rtv, grey, ng, grid);
    const float red[4] = {0.9f, 0.1f, 0.1f, 1};
    const D3D12_RECT hud = {16, 16, LONG(16 + width / 6), LONG(16 + height / 16)};
    list->ClearRenderTargetView(rtv, red, 1, &hud);
    const LONG box = LONG(height / 8);
    const double ph = std::fmod(t * 0.5, 2.0);
    const LONG x = LONG((ph < 1.0 ? ph : 2.0 - ph) * double(width - box));
    const float white[4] = {1, 1, 1, 1};
    const D3D12_RECT mv = {x, LONG(height / 4), x + box, LONG(height / 4) + box};
    list->ClearRenderTargetView(rtv, white, 1, &mv);
    b.Transition.StateBefore = D3D12_RESOURCE_STATE_RENDER_TARGET;
    b.Transition.StateAfter = D3D12_RESOURCE_STATE_PRESENT;
    list->ResourceBarrier(1, &b);
    list->Close();
    ID3D12CommandList* lists[1] = {list};
    queue->ExecuteCommandLists(1, lists);
    const double p0 = secs();
    HRESULT hr = sc->Present(0, 0);
    const double pd = secs() - p0;
    psum += pd;
    if (pd > pmax) pmax = pd;
    queue->Signal(fence, ++fv);
    done_at[i] = fv;
    if (FAILED(hr)) {
      if (++failed <= 5) std::printf("Present: 0x%08lx\n", hr);
      if (failed > 100) break;
    }
    frames++;
  }
  queue->Signal(fence, ++fv);
  fence->SetEventOnCompletion(fv, ev);
  WaitForSingleObject(ev, 2000);
  const double total = secs();
  std::printf("%u frames in %.2f s: %.1f fps; Present avg %.3f ms, max %.3f ms; %u failed\n",
              frames, total, frames / total, frames ? psum / frames * 1000.0 : 0.0,
              pmax * 1000.0, failed);
  return 0;
}
