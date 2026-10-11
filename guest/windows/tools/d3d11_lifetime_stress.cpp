// d3d11_lifetime_stress: churn the lifetimes the driver must get right, deterministically.
//
//   d3d11_lifetime_stress.exe [SECONDS=60 [SEED=1 [WINDOWS=6 [FRONTEND=keep|0|1 [CHILDREN=1]]]]]
//
// Every frame, with a seeded generator (the same SEED gives the same sequence of sizes, formats
// and operations):
//   * textures of random sizes are created, filled (UpdateSubresource, and a DYNAMIC one through
//     Map(WRITE_DISCARD)), copied (CopyResource, CopySubresourceRegion with boxes) into a second
//     texture and into the windows' back buffers, and released right away, while the copies that
//     read them are still queued;
//   * WINDOWS small windowed swap chains (alternately FLIP_SEQUENTIAL and FLIP_DISCARD, all
//     different sizes) present with Present(0, 0): more distinct presented images than the KMD's
//     copy-engine route caches, so its producer mappings are evicted and re-made all the time;
//   * every 90 frames one window's swap chain is destroyed and made again at another size, and
//     every 45 frames one is resized (ResizeBuffers);
//   * every 2 s a CHILD process is started (at most CHILDREN at once) that opens its own device,
//     window and swap chain, presents 40 frames and exits: an RM client comes and goes (the KMD's
//     client table changes) while its last presents may still be copied;
//   * every 16th frame one copy is read back through a staging texture and compared with what was
//     uploaded: a lifetime bug in the UMD or the threaded front end shows up as a mismatch.
//
// FRONTEND sets HELIOS_D3D11_FRONTEND for this process and its children before the device is made
// (keep: the registry's UmdThreadedFrontend decides).
//
// Per second it prints frames, textures made, copies, presents, swap chains remade, children and
// mismatches. Exit code 0: no mismatch and no device removal. The oracle for GPU faults is the
// host: `sudo dmesg | grep Xid` must not grow while it runs.
//
// Build (one line): x86_64-w64-mingw32-g++ -std=c++17 -O2 -static d3d11_lifetime_stress.cpp
//   -o d3d11_lifetime_stress.exe -ld3d11 -ldxgi -luser32 -lgdi32
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11.h>
#include <dxgi1_4.h>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <vector>

static double now_ms() {
  static LARGE_INTEGER f = [] { LARGE_INTEGER x; QueryPerformanceFrequency(&x); return x; }();
  LARGE_INTEGER t;
  QueryPerformanceCounter(&t);
  return 1000.0 * double(t.QuadPart) / double(f.QuadPart);
}

template <class T> static void release(T *&p) {
  if (p) p->Release();
  p = nullptr;
}

// xorshift64*: deterministic across runs and compilers.
struct Rng {
  unsigned long long s;
  explicit Rng(unsigned long long seed) : s(seed * 0x9E3779B97F4A7C15ull | 1) {}
  unsigned next() {
    s ^= s >> 12;
    s ^= s << 25;
    s ^= s >> 27;
    return unsigned((s * 0x2545F4914F6CDD1Dull) >> 32);
  }
  unsigned range(unsigned lo, unsigned hi) { return lo + next() % (hi - lo + 1); }
};

static LRESULT CALLBACK wndproc(HWND h, UINT m, WPARAM w, LPARAM l) {
  if (m == WM_CLOSE) return 0;  // the tool decides when windows go
  return DefWindowProcW(h, m, w, l);
}

static HWND make_window(int x, int y, UINT w, UINT h, const wchar_t *title) {
  static bool registered = false;
  if (!registered) {
    WNDCLASSW wc = {};
    wc.lpfnWndProc = wndproc;
    wc.hInstance = GetModuleHandleW(nullptr);
    wc.lpszClassName = L"HeliosLifetimeStress";
    wc.hCursor = LoadCursor(nullptr, IDC_ARROW);
    RegisterClassW(&wc);
    registered = true;
  }
  RECT r = {0, 0, LONG(w), LONG(h)};
  AdjustWindowRect(&r, WS_OVERLAPPEDWINDOW, FALSE);
  HWND hwnd = CreateWindowExW(0, L"HeliosLifetimeStress", title, WS_OVERLAPPEDWINDOW | WS_VISIBLE, x, y,
                              r.right - r.left, r.bottom - r.top, nullptr, nullptr, GetModuleHandleW(nullptr), nullptr);
  return hwnd;
}

static void pump() {
  MSG msg;
  while (PeekMessageW(&msg, nullptr, 0, 0, PM_REMOVE)) {
    TranslateMessage(&msg);
    DispatchMessageW(&msg);
  }
}

static bool make_device(ID3D11Device **dev, ID3D11DeviceContext **ctx) {
  D3D_FEATURE_LEVEL fl = D3D_FEATURE_LEVEL_11_0;
  HRESULT hr = D3D11CreateDevice(nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr, D3D11_CREATE_DEVICE_BGRA_SUPPORT, &fl, 1,
                                 D3D11_SDK_VERSION, dev, nullptr, ctx);
  if (FAILED(hr)) std::printf("[FAIL] D3D11CreateDevice: 0x%08lx\n", static_cast<unsigned long>(hr));
  return SUCCEEDED(hr);
}

struct Win {
  HWND hwnd = nullptr;
  IDXGISwapChain1 *sc = nullptr;
  UINT w = 0, h = 0;
  bool sequential = true;
};

static bool make_swapchain(ID3D11Device *dev, IDXGIFactory2 *f, Win &win) {
  DXGI_SWAP_CHAIN_DESC1 d = {};
  d.Width = win.w;
  d.Height = win.h;
  d.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
  d.SampleDesc.Count = 1;
  d.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
  d.BufferCount = 2;
  d.SwapEffect = win.sequential ? DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL : DXGI_SWAP_EFFECT_FLIP_DISCARD;
  HRESULT hr = f->CreateSwapChainForHwnd(dev, win.hwnd, &d, nullptr, nullptr, &win.sc);
  if (FAILED(hr)) std::printf("[FAIL] CreateSwapChainForHwnd %ux%u: 0x%08lx\n", win.w, win.h, static_cast<unsigned long>(hr));
  return SUCCEEDED(hr);
}

static UINT32 pattern(unsigned salt, UINT x, UINT y) { return 0xff000000u | ((x * 7u + y * 13u + salt * 101u) & 0xffffffu); }

static ID3D11Texture2D *texture(ID3D11Device *dev, UINT w, UINT h, D3D11_USAGE usage, UINT bind, UINT cpu) {
  D3D11_TEXTURE2D_DESC d = {};
  d.Width = w;
  d.Height = h;
  d.MipLevels = 1;
  d.ArraySize = 1;
  d.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
  d.SampleDesc.Count = 1;
  d.Usage = usage;
  d.BindFlags = bind;
  d.CPUAccessFlags = cpu;
  ID3D11Texture2D *t = nullptr;
  dev->CreateTexture2D(&d, nullptr, &t);
  return t;
}

// ---- the child: an RM client that comes and goes ----------------------------------------------

static int child(unsigned seed) {
  ID3D11Device *dev = nullptr;
  ID3D11DeviceContext *ctx = nullptr;
  if (!make_device(&dev, &ctx)) return 1;
  IDXGIFactory2 *f = nullptr;
  CreateDXGIFactory1(__uuidof(IDXGIFactory2), reinterpret_cast<void **>(&f));
  Rng rng(seed);
  Win win;
  win.w = rng.range(200, 500);
  win.h = rng.range(150, 400);
  win.sequential = rng.next() & 1;
  win.hwnd = make_window(int(rng.range(0, 800)), int(rng.range(0, 500)), win.w, win.h, L"lifetime-stress child");
  if (!f || !win.hwnd || !make_swapchain(dev, f, win)) return 1;
  std::vector<UINT32> px(size_t(win.w) * win.h);
  for (UINT frame = 0; frame < 40; frame++) {
    pump();
    ID3D11Texture2D *bb = nullptr;
    win.sc->GetBuffer(0, __uuidof(ID3D11Texture2D), reinterpret_cast<void **>(&bb));
    for (UINT y = 0; y < win.h; y++)
      for (UINT x = 0; x < win.w; x++) px[size_t(y) * win.w + x] = pattern(seed + frame, x, y);
    if (bb) ctx->UpdateSubresource(bb, 0, nullptr, px.data(), win.w * 4, 0);
    release(bb);
    win.sc->Present(0, 0);
  }
  // Exit without waiting for the GPU: the process' client goes while its last copies may run.
  ExitProcess(0);
}

// ---- the parent ---------------------------------------------------------------------------------

int main(int argc, char **argv) {
  if (argc >= 3 && !std::strcmp(argv[1], "child")) return child(unsigned(std::strtoul(argv[2], nullptr, 10)));
  const double seconds = argc > 1 ? std::atof(argv[1]) : 60.0;
  const unsigned seed = argc > 2 ? unsigned(std::strtoul(argv[2], nullptr, 10)) : 1u;
  const UINT nwin = argc > 3 ? UINT(std::atoi(argv[3])) : 6u;
  const char *frontend = argc > 4 ? argv[4] : "keep";
  const UINT max_children = argc > 5 ? UINT(std::atoi(argv[5])) : 1u;
  if (!std::strcmp(frontend, "0") || !std::strcmp(frontend, "1")) SetEnvironmentVariableA("HELIOS_D3D11_FRONTEND", frontend);
  std::printf("d3d11_lifetime_stress: %.0f s, seed %u, %u windows, front end %s, %u children\n", seconds, seed, nwin,
              frontend, max_children);
  ID3D11Device *dev = nullptr;
  ID3D11DeviceContext *ctx = nullptr;
  if (!make_device(&dev, &ctx)) return 1;
  IDXGIFactory2 *f = nullptr;
  if (FAILED(CreateDXGIFactory1(__uuidof(IDXGIFactory2), reinterpret_cast<void **>(&f)))) return 1;
  Rng rng(seed);
  std::vector<Win> wins(nwin);
  for (UINT i = 0; i < nwin; i++) {
    Win &w = wins[i];
    w.w = 240 + 40 * i + rng.range(0, 30);
    w.h = 180 + 24 * i + rng.range(0, 30);
    w.sequential = (i & 1) == 0;
    w.hwnd = make_window(int(40 + 60 * i), int(40 + 40 * i), w.w, w.h, L"lifetime-stress");
    if (!w.hwnd || !make_swapchain(dev, f, w)) return 1;
  }
  ID3D11Texture2D *staging = texture(dev, 1024, 1024, D3D11_USAGE_STAGING, 0, D3D11_CPU_ACCESS_READ);
  ID3D11Texture2D *dynamic = texture(dev, 512, 512, D3D11_USAGE_DYNAMIC, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_WRITE);
  if (!staging || !dynamic) {
    std::printf("[FAIL] staging/dynamic texture\n");
    return 1;
  }
  std::vector<UINT32> up(1024 * 1024);
  std::vector<PROCESS_INFORMATION> kids;
  unsigned long long frames = 0, made = 0, copies = 0, presents = 0, remade = 0, resized = 0, spawned = 0;
  unsigned long long mismatches = 0, checks = 0;
  bool removed = false;
  const double t0 = now_ms();
  double next_report = t0 + 1000.0, next_child = t0 + 2000.0;
  char exe[MAX_PATH];
  GetModuleFileNameA(nullptr, exe, MAX_PATH);
  while (now_ms() - t0 < seconds * 1000.0 && !removed) {
    pump();
    frames++;
    // Textures made, filled, copied and released while the copies are queued.
    const UINT n = rng.range(4, 10);
    for (UINT i = 0; i < n; i++) {
      const UINT w = rng.range(16, 1024), h = rng.range(16, 1024);
      const unsigned salt = rng.next();
      ID3D11Texture2D *a = texture(dev, w, h, D3D11_USAGE_DEFAULT, D3D11_BIND_SHADER_RESOURCE, 0);
      ID3D11Texture2D *b = texture(dev, w, h, D3D11_USAGE_DEFAULT, D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET, 0);
      if (!a || !b) {
        release(a);
        release(b);
        continue;
      }
      made += 2;
      for (UINT y = 0; y < h; y++)
        for (UINT x = 0; x < w; x++) up[size_t(y) * w + x] = pattern(salt, x, y);
      ctx->UpdateSubresource(a, 0, nullptr, up.data(), w * 4, 0);
      ctx->CopyResource(b, a);
      copies++;
      // A box of it into a back buffer.
      Win &win = wins[rng.range(0, nwin - 1)];
      ID3D11Texture2D *bb = nullptr;
      if (SUCCEEDED(win.sc->GetBuffer(0, __uuidof(ID3D11Texture2D), reinterpret_cast<void **>(&bb)))) {
        const UINT bw = w < win.w ? w : win.w, bh = h < win.h ? h : win.h;
        D3D11_BOX box = {0, 0, 0, bw, bh, 1};
        ctx->CopySubresourceRegion(bb, 0, rng.range(0, win.w - bw), rng.range(0, win.h - bh), 0, b, 0, &box);
        copies++;
        release(bb);
      }
      // Every 16th frame, the first texture is read back and compared.
      if (i == 0 && frames % 16 == 0) {
        D3D11_BOX box = {0, 0, 0, w, h, 1};
        ctx->CopySubresourceRegion(staging, 0, 0, 0, 0, b, 0, &box);
        release(a);
        release(b);  // released BEFORE the readback: the copy must keep what it reads alive
        D3D11_MAPPED_SUBRESOURCE m;
        if (SUCCEEDED(ctx->Map(staging, 0, D3D11_MAP_READ, 0, &m))) {
          checks++;
          bool ok = true;
          for (UINT y = 0; y < h && ok; y += 7)
            for (UINT x = 0; x < w && ok; x += 5)
              if (static_cast<const UINT32 *>(m.pData)[size_t(y) * (m.RowPitch / 4) + x] != pattern(salt, x, y)) {
                ok = false;
                std::printf("[MISMATCH] frame %llu: %ux%u copy differs at (%u,%u)\n", frames, w, h, x, y);
              }
          if (!ok) mismatches++;
          ctx->Unmap(staging, 0);
        }
      }
      release(a);
      release(b);
    }
    // The DYNAMIC texture renamed and read by a copy.
    D3D11_MAPPED_SUBRESOURCE m;
    if (SUCCEEDED(ctx->Map(dynamic, 0, D3D11_MAP_WRITE_DISCARD, 0, &m))) {
      std::memset(m.pData, int(frames & 0xff), size_t(m.RowPitch) * 512);
      ctx->Unmap(dynamic, 0);
      D3D11_BOX box = {0, 0, 0, 512, 512, 1};
      ctx->CopySubresourceRegion(staging, 0, 0, 0, 0, dynamic, 0, &box);
      copies++;
    }
    // Swap chains: remade, resized, presented.
    if (frames % 90 == 0) {
      Win &w = wins[rng.range(0, nwin - 1)];
      release(w.sc);
      ctx->ClearState();
      ctx->Flush();
      w.w = rng.range(200, 640);
      w.h = rng.range(150, 480);
      SetWindowPos(w.hwnd, nullptr, 0, 0, int(w.w), int(w.h), SWP_NOMOVE | SWP_NOZORDER);
      if (!make_swapchain(dev, f, w)) return 1;
      remade++;
    } else if (frames % 45 == 0) {
      Win &w = wins[rng.range(0, nwin - 1)];
      w.w = rng.range(200, 640);
      w.h = rng.range(150, 480);
      ctx->ClearState();
      if (SUCCEEDED(w.sc->ResizeBuffers(0, w.w, w.h, DXGI_FORMAT_UNKNOWN, 0))) resized++;
    }
    for (Win &w : wins) {
      HRESULT hr = w.sc->Present(0, 0);
      presents++;
      if (hr == DXGI_ERROR_DEVICE_REMOVED || hr == DXGI_ERROR_DEVICE_RESET) {
        std::printf("[FAIL] Present: device removed (0x%08lx, reason 0x%08lx)\n", static_cast<unsigned long>(hr),
                    static_cast<unsigned long>(dev->GetDeviceRemovedReason()));
        removed = true;
        break;
      }
    }
    // Children come and go.
    for (size_t k = 0; k < kids.size();) {
      if (WaitForSingleObject(kids[k].hProcess, 0) == WAIT_OBJECT_0) {
        CloseHandle(kids[k].hProcess);
        CloseHandle(kids[k].hThread);
        kids.erase(kids.begin() + long(k));
      } else {
        k++;
      }
    }
    if (max_children && now_ms() >= next_child && kids.size() < max_children) {
      next_child = now_ms() + 2000.0;
      char cmd[MAX_PATH + 64];
      std::snprintf(cmd, sizeof(cmd), "\"%s\" child %u", exe, seed * 1000u + unsigned(spawned));
      STARTUPINFOA si = {sizeof(si)};
      PROCESS_INFORMATION pi = {};
      if (CreateProcessA(nullptr, cmd, nullptr, nullptr, FALSE, 0, nullptr, nullptr, &si, &pi)) {
        kids.push_back(pi);
        spawned++;
      }
    }
    if (now_ms() >= next_report) {
      next_report += 1000.0;
      std::printf("t=%3.0fs frames %llu textures %llu copies %llu presents %llu remade %llu resized %llu children %llu "
                  "checks %llu mismatches %llu\n",
                  (now_ms() - t0) / 1000.0, frames, made, copies, presents, remade, resized, spawned, checks, mismatches);
      std::fflush(stdout);
    }
  }
  for (PROCESS_INFORMATION &pi : kids) {
    WaitForSingleObject(pi.hProcess, 10000);
    CloseHandle(pi.hProcess);
    CloseHandle(pi.hThread);
  }
  std::printf("%s: frames %llu textures %llu copies %llu presents %llu remade %llu resized %llu children %llu checks %llu "
              "mismatches %llu%s\n",
              mismatches || removed ? "[FAIL]" : "[PASS]", frames, made, copies, presents, remade, resized, spawned, checks,
              mismatches, removed ? " (device removed)" : "");
  for (Win &w : wins) {
    release(w.sc);
    DestroyWindow(w.hwnd);
  }
  release(staging);
  release(dynamic);
  release(f);
  release(ctx);
  release(dev);
  return mismatches || removed ? 2 : 0;
}
