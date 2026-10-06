// handoff_bench: the cost of one cross-process hand-off of a shared D3D11
// texture, every step timed (S6, docs/shared-surfaces.md section 4).
//
//   handoff_bench.exe [MODE=nt-keyed [FRAMES=300 [W=1280 H=720 [WORK=clear|band|none [CHECK=1]]]]]
//
// MODE
//   kmt-keyed  legacy DXGI shared handle (GetSharedHandle) + keyed mutex
//   nt-keyed   NT handle by name + keyed mutex
//   nt-fence   NT handle, no mutex, a shared ID3D11Fence: producer Signal(n)
//              + Flush, consumer ID3D11DeviceContext4::Wait(n)
//   kmt-flush  legacy shared handle, no sync object: producer Flush + event
//              query, consumer opens and copies (the "plain shared" case)
//
// A (this process) is the producer, B (started by A) the consumer. They
// ping-pong FRAMES hand-offs:
//   A: AcquireSync(0) | WORK into the shared texture | Flush | ReleaseSync(1)
//   B: AcquireSync(1) | CopyResource shared -> private | event query wait |
//      (CHECK: read back one texel of frame n) | ReleaseSync(0)
// For nt-fence the order comes from the fence and a shared counter instead.
//
// Per step it prints median / p90 / p99 / max in microseconds, plus the
// round trip per hand-off and hand-offs per second. CHECK=1 compares one
// texel (frame colour) in B per hand-off: stale counts a hand-off whose read
// saw an older frame (ordering broken). Exit 0 if no stale reads and no
// timeouts.
//
// HELIOS_ICD=nvk / venus selects the UMD backend (inherited by B).
//
// Build: x86_64-w64-mingw32-g++ -std=c++17 -O2 -static handoff_bench.cpp
//          -o handoff_bench.exe -ld3d11 -ldxgi -luuid
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11_4.h>
#include <dxgi1_2.h>

#include <algorithm>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <vector>

static const wchar_t kName[] = L"Local\\helios_handoff_bench";
static const wchar_t kShm[] = L"Local\\helios_handoff_bench_shm";

static double now_us() {
  static LARGE_INTEGER f = [] { LARGE_INTEGER x; QueryPerformanceFrequency(&x); return x; }();
  LARGE_INTEGER t;
  QueryPerformanceCounter(&t);
  return 1e6 * double(t.QuadPart) / double(f.QuadPart);
}

struct Shm {
  volatile LONG64 produced;   // last frame the producer signalled (nt-fence, kmt-flush)
  volatile LONG64 consumed;   // last frame the consumer finished reading
  volatile LONG64 kmt_handle; // legacy shared handle value
  volatile LONG64 fence_handle;
  volatile LONG abort;
};

static Shm *shm() {
  static Shm *p = [] {
    HANDLE m = CreateFileMappingW(INVALID_HANDLE_VALUE, nullptr, PAGE_READWRITE, 0, 4096, kShm);
    return m ? static_cast<Shm *>(MapViewOfFile(m, FILE_MAP_ALL_ACCESS, 0, 0, 4096)) : nullptr;
  }();
  return p;
}

struct Series {
  const char *name;
  std::vector<double> v;
  void add(double x) { v.push_back(x); }
  void print() {
    if (v.empty()) return;
    std::vector<double> s = v;
    std::sort(s.begin(), s.end());
    auto at = [&](double q) { return s[std::min(s.size() - 1, size_t(q * double(s.size())))]; };
    std::printf("  %-22s n=%-5zu med %9.1f  p90 %9.1f  p99 %9.1f  max %9.1f us\n", name, s.size(), at(0.5), at(0.9),
                at(0.99), s.back());
  }
};

static UINT32 frame_color(UINT n) { return 0xff000000u | ((n * 37u) & 0xff) << 16 | ((n * 11u) & 0xff) << 8 | (n & 0xff); }

static bool make_device(ID3D11Device5 **dev, ID3D11DeviceContext4 **ctx) {
  ID3D11Device *d = nullptr;
  ID3D11DeviceContext *c = nullptr;
  D3D_FEATURE_LEVEL fl = D3D_FEATURE_LEVEL_11_1;
  HRESULT hr = D3D11CreateDevice(nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr, D3D11_CREATE_DEVICE_BGRA_SUPPORT, &fl, 1,
                                 D3D11_SDK_VERSION, &d, nullptr, &c);
  if (FAILED(hr)) {
    std::printf("[FAIL] D3D11CreateDevice 0x%08lx\n", (unsigned long)hr);
    return false;
  }
  bool ok = SUCCEEDED(d->QueryInterface(__uuidof(ID3D11Device5), (void **)dev)) &&
            SUCCEEDED(c->QueryInterface(__uuidof(ID3D11DeviceContext4), (void **)ctx));
  d->Release();
  c->Release();
  return ok;
}

// Wait on the CPU until everything submitted so far has completed.
static bool gpu_idle(ID3D11Device5 *dev, ID3D11DeviceContext4 *ctx, double cap_us) {
  D3D11_QUERY_DESC qd = {D3D11_QUERY_EVENT, 0};
  ID3D11Query *q = nullptr;
  if (FAILED(dev->CreateQuery(&qd, &q))) return false;
  ctx->End(q);
  BOOL done = FALSE;
  const double t = now_us();
  while (ctx->GetData(q, &done, sizeof(done), 0) != S_OK) {
    if (now_us() - t > cap_us) break;
    YieldProcessor();
  }
  q->Release();
  return done != FALSE;
}

// ---- consumer --------------------------------------------------------------

static int consumer(const char *mode, UINT frames, UINT w, UINT h, bool check) {
  ID3D11Device5 *dev = nullptr;
  ID3D11DeviceContext4 *ctx = nullptr;
  if (!make_device(&dev, &ctx)) return 2;
  Shm *m = shm();
  const bool kmt = !std::strncmp(mode, "kmt", 3);
  const bool keyed = std::strstr(mode, "keyed") != nullptr;
  const bool fenced = !std::strcmp(mode, "nt-fence");
  ID3D11Texture2D *tex = nullptr;
  double t = now_us();
  HRESULT hr = kmt ? dev->OpenSharedResource(HANDLE(m->kmt_handle), __uuidof(ID3D11Texture2D), (void **)&tex)
                   : dev->OpenSharedResourceByName(kName, DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE,
                                                   __uuidof(ID3D11Texture2D), (void **)&tex);
  std::printf("  B: open %s: 0x%08lx (%.0f us)\n", kmt ? "kmt" : "nt", (unsigned long)hr, now_us() - t);
  if (FAILED(hr)) {
    m->abort = 1;
    return 2;
  }
  IDXGIKeyedMutex *km = nullptr;
  if (keyed) tex->QueryInterface(__uuidof(IDXGIKeyedMutex), (void **)&km);
  ID3D11Fence *fence = nullptr;
  if (fenced) {
    hr = dev->OpenSharedFence(HANDLE(m->fence_handle), __uuidof(ID3D11Fence), (void **)&fence);
    std::printf("  B: OpenSharedFence: 0x%08lx\n", (unsigned long)hr);
    if (FAILED(hr)) {
      m->abort = 1;
      return 2;
    }
  }
  D3D11_TEXTURE2D_DESC td;
  tex->GetDesc(&td);
  td.MiscFlags = 0;
  td.BindFlags = D3D11_BIND_SHADER_RESOURCE;
  ID3D11Texture2D *priv = nullptr;
  dev->CreateTexture2D(&td, nullptr, &priv);
  D3D11_TEXTURE2D_DESC sd = td;
  sd.Width = sd.Height = 1;
  sd.BindFlags = 0;
  sd.Usage = D3D11_USAGE_STAGING;
  sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
  ID3D11Texture2D *staging = nullptr;
  dev->CreateTexture2D(&sd, nullptr, &staging);

  Series acq{"B AcquireSync(1)"}, fwait{"B fence Wait+idle"}, copy{"B copy+GPU idle"}, rel{"B ReleaseSync(0)"},
      readback{"B 1-texel check"};
  UINT stale = 0, timeouts = 0;
  for (UINT n = 1; n <= frames && !m->abort; n++) {
    if (keyed) {
      t = now_us();
      hr = km->AcquireSync(1, 5000);
      acq.add(now_us() - t);
      if (hr != S_OK) {
        timeouts++;
        std::printf("  B: AcquireSync(1) frame %u: 0x%08lx\n", n, (unsigned long)hr);
        if (timeouts > 3) break;
        continue;
      }
    } else {
      const double t0 = now_us();
      while (m->produced < LONG64(n) && !m->abort && now_us() - t0 < 5e6) YieldProcessor();
      if (m->produced < LONG64(n)) {
        timeouts++;
        break;
      }
      if (fenced) {
        t = now_us();
        ctx->Wait(fence, n);
        if (!gpu_idle(dev, ctx, 5e6)) timeouts++;
        fwait.add(now_us() - t);
      }
    }
    t = now_us();
    ctx->CopyResource(priv, tex);
    if (!gpu_idle(dev, ctx, 5e6)) timeouts++;
    copy.add(now_us() - t);
    if (check) {
      t = now_us();
      D3D11_BOX box = {w / 2, h / 2, 0, w / 2 + 1, h / 2 + 1, 1};
      ctx->CopySubresourceRegion(staging, 0, 0, 0, 0, priv, 0, &box);
      D3D11_MAPPED_SUBRESOURCE map;
      if (SUCCEEDED(ctx->Map(staging, 0, D3D11_MAP_READ, 0, &map))) {
        const UINT32 got = *static_cast<UINT32 *>(map.pData);
        ctx->Unmap(staging, 0);
        if (got != frame_color(n)) {
          if (stale < 5) std::printf("  B: frame %u stale: got %08x want %08x\n", n, got, frame_color(n));
          stale++;
        }
      }
      readback.add(now_us() - t);
    }
    if (keyed) {
      t = now_us();
      km->ReleaseSync(0);
      rel.add(now_us() - t);
    }
    m->consumed = n;
  }
  std::printf("  B: %u stale, %u timeouts\n", stale, timeouts);
  acq.print();
  fwait.print();
  copy.print();
  readback.print();
  rel.print();
  if (km) km->Release();
  if (fence) fence->Release();
  staging->Release();
  priv->Release();
  tex->Release();
  ctx->Release();
  dev->Release();
  return (stale || timeouts) ? 1 : 0;
}

// ---- producer --------------------------------------------------------------

int main(int argc, char **argv) {
  setvbuf(stdout, nullptr, _IONBF, 0);
  if (argc >= 7 && !std::strcmp(argv[1], "consumer"))
    return consumer(argv[2], UINT(std::atoi(argv[3])), UINT(std::atoi(argv[4])), UINT(std::atoi(argv[5])),
                    std::atoi(argv[6]) != 0);
  const char *mode = argc > 1 ? argv[1] : "nt-keyed";
  const UINT frames = argc > 2 ? UINT(std::atoi(argv[2])) : 300;
  const UINT w = argc > 4 ? UINT(std::atoi(argv[3])) : 1280, h = argc > 4 ? UINT(std::atoi(argv[4])) : 720;
  const char *work = argc > 5 ? argv[5] : "clear";
  const bool check = argc > 6 ? std::atoi(argv[6]) != 0 : true;
  const bool kmt = !std::strncmp(mode, "kmt", 3);
  const bool keyed = std::strstr(mode, "keyed") != nullptr;
  const bool fenced = !std::strcmp(mode, "nt-fence");
  if (!kmt && !keyed && !fenced) {
    std::printf("unknown mode %s\n", mode);
    return 2;
  }
  std::printf("handoff_bench %s, %u frames, %ux%u, work %s, check %d, HELIOS_ICD=%s\n", mode, frames, w, h, work,
              check, std::getenv("HELIOS_ICD") ? std::getenv("HELIOS_ICD") : "(unset)");
  Shm *m = shm();
  if (!m) return 2;
  std::memset((void *)m, 0, sizeof(*m));

  ID3D11Device5 *dev = nullptr;
  ID3D11DeviceContext4 *ctx = nullptr;
  if (!make_device(&dev, &ctx)) return 2;
  D3D11_TEXTURE2D_DESC td = {};
  td.Width = w;
  td.Height = h;
  td.MipLevels = 1;
  td.ArraySize = 1;
  td.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
  td.SampleDesc.Count = 1;
  td.Usage = D3D11_USAGE_DEFAULT;
  td.BindFlags = D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET;
  td.MiscFlags = kmt ? (keyed ? D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX : D3D11_RESOURCE_MISC_SHARED)
                     : (D3D11_RESOURCE_MISC_SHARED_NTHANDLE |
                        (keyed ? D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX : D3D11_RESOURCE_MISC_SHARED));
  ID3D11Texture2D *tex = nullptr;
  HRESULT hr = dev->CreateTexture2D(&td, nullptr, &tex);
  if (FAILED(hr)) {
    std::printf("[FAIL] CreateTexture2D 0x%08lx\n", (unsigned long)hr);
    return 2;
  }
  HANDLE nt = nullptr;
  if (kmt) {
    IDXGIResource *r = nullptr;
    tex->QueryInterface(__uuidof(IDXGIResource), (void **)&r);
    HANDLE hs = nullptr;
    hr = r->GetSharedHandle(&hs);
    r->Release();
    m->kmt_handle = LONG64(reinterpret_cast<uintptr_t>(hs));
  } else {
    IDXGIResource1 *r = nullptr;
    tex->QueryInterface(__uuidof(IDXGIResource1), (void **)&r);
    hr = r->CreateSharedHandle(nullptr, DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE, kName, &nt);
    r->Release();
  }
  if (FAILED(hr)) {
    std::printf("[FAIL] share 0x%08lx\n", (unsigned long)hr);
    return 2;
  }
  IDXGIKeyedMutex *km = nullptr;
  if (keyed) tex->QueryInterface(__uuidof(IDXGIKeyedMutex), (void **)&km);
  ID3D11Fence *fence = nullptr;
  HANDLE fence_nt = nullptr;
  PROCESS_INFORMATION pi = {};
  ID3D11RenderTargetView *rtv = nullptr;
  dev->CreateRenderTargetView(tex, nullptr, &rtv);
  if (fenced) {
    hr = dev->CreateFence(0, D3D11_FENCE_FLAG_SHARED, __uuidof(ID3D11Fence), (void **)&fence);
    if (SUCCEEDED(hr)) hr = fence->CreateSharedHandle(nullptr, GENERIC_ALL, nullptr, &fence_nt);
    if (FAILED(hr)) {
      std::printf("[FAIL] shared fence 0x%08lx\n", (unsigned long)hr);
      return 2;
    }
  }
  // The texture starts at frame 0's colour, owned by key 0.
  if (keyed) km->AcquireSync(0, INFINITE);
  const float c0[4] = {0, 0, 0, 1};
  ctx->ClearRenderTargetView(rtv, c0);
  ctx->Flush();
  gpu_idle(dev, ctx, 5e6);
  if (keyed) km->ReleaseSync(0);

  char self[MAX_PATH], cmd[MAX_PATH + 128];
  GetModuleFileNameA(nullptr, self, sizeof(self));
  std::snprintf(cmd, sizeof(cmd), "\"%s\" consumer %s %u %u %u %d", self, mode, frames, w, h, check ? 1 : 0);
  STARTUPINFOA si = {};
  si.cb = sizeof(si);
  if (fenced) {
    // B gets the fence handle by duplication into its process.
    if (!CreateProcessA(nullptr, cmd, nullptr, nullptr, FALSE, CREATE_SUSPENDED, nullptr, nullptr, &si, &pi)) return 2;
    HANDLE theirs = nullptr;
    DuplicateHandle(GetCurrentProcess(), fence_nt, pi.hProcess, &theirs, 0, FALSE, DUPLICATE_SAME_ACCESS);
    m->fence_handle = LONG64(reinterpret_cast<uintptr_t>(theirs));
    ResumeThread(pi.hThread);
  } else if (!CreateProcessA(nullptr, cmd, nullptr, nullptr, FALSE, 0, nullptr, nullptr, &si, &pi)) {
    std::printf("[FAIL] start consumer %lu\n", GetLastError());
    return 2;
  }

  Series acq{"A AcquireSync(0)"}, wk{"A work (record)"}, fl{"A Flush"}, rel{"A ReleaseSync(1)"},
      sig{"A Signal+Flush"}, idle{"A GPU idle (kmt-flush)"}, sigcpu{"A fence done on CPU"}, rt{"round trip"};
  std::vector<UINT32> band(size_t(w) * 8);
  UINT timeouts = 0;
  HANDLE ev = CreateEventW(nullptr, FALSE, FALSE, nullptr);
  const double start = now_us();
  double last = start;
  for (UINT n = 1; n <= frames && !m->abort; n++) {
    double t;
    if (keyed) {
      t = now_us();
      hr = km->AcquireSync(0, 5000);
      acq.add(now_us() - t);
      if (hr != S_OK) {
        timeouts++;
        std::printf("  A: AcquireSync(0) frame %u: 0x%08lx\n", n, (unsigned long)hr);
        if (timeouts > 3) break;
        continue;
      }
    } else {
      // Wait until B read the previous frame (no sync object orders this).
      const double t0 = now_us();
      while (m->consumed < LONG64(n - 1) && !m->abort && now_us() - t0 < 5e6) YieldProcessor();
    }
    t = now_us();
    const UINT32 col = frame_color(n);
    if (!std::strcmp(work, "clear")) {
      const float c[4] = {float((col >> 16) & 0xff) / 255.f, float((col >> 8) & 0xff) / 255.f, float(col & 0xff) / 255.f,
                          1.f};
      ctx->ClearRenderTargetView(rtv, c);
    } else if (!std::strcmp(work, "band")) {
      std::fill(band.begin(), band.end(), col);
      D3D11_BOX box = {0, h / 2 - 4, 0, w, h / 2 + 4, 1};
      ctx->UpdateSubresource(tex, 0, &box, band.data(), w * 4, 0);
    }
    wk.add(now_us() - t);
    if (keyed) {
      t = now_us();
      ctx->Flush();
      fl.add(now_us() - t);
      t = now_us();
      km->ReleaseSync(1);
      rel.add(now_us() - t);
    } else if (fenced) {
      t = now_us();
      ctx->Signal(fence, n);
      ctx->Flush();
      sig.add(now_us() - t);
      t = now_us();
      fence->SetEventOnCompletion(n, ev);
      if (WaitForSingleObject(ev, 5000) != WAIT_OBJECT_0) timeouts++;
      sigcpu.add(now_us() - t);
      m->produced = n;
    } else {
      t = now_us();
      ctx->Flush();
      fl.add(now_us() - t);
      t = now_us();
      gpu_idle(dev, ctx, 5e6);
      idle.add(now_us() - t);
      m->produced = n;
    }
    const double now = now_us();
    rt.add(now - last);
    last = now;
  }
  const double secs = (now_us() - start) / 1e6;
  WaitForSingleObject(pi.hProcess, 20000);
  DWORD code = 99;
  GetExitCodeProcess(pi.hProcess, &code);
  std::printf("  A: %u frames in %.2f s = %.1f hand-offs/s, %u timeouts\n", frames, secs, frames / secs, timeouts);
  acq.print();
  wk.print();
  fl.print();
  rel.print();
  sig.print();
  sigcpu.print();
  idle.print();
  rt.print();
  std::printf("  consumer exit %lu\n", code);
  const bool ok = code == 0 && timeouts == 0;
  std::printf("%s\n", ok ? "HANDOFF-BENCH PASS" : "HANDOFF-BENCH FAIL");
  return ok ? 0 : 1;
}
