// d3d11_share: one D3D11 texture shared between two processes, the normal way
// (guest/windows/docs/shared-surfaces.md, dxvk-on-nvk S6). The opener goes
// through D3DKMTOpenResource / DxgkDdiOpenAllocation like any app.
//
//   d3d11_share.exe [nt|nt-keyed|kmt] [width=1920 height=1080]
//   d3d11_share.exe keyed-load [width height [rounds=20 [copies=400 [perf]]]]
//
//   d3d11_share.exe churn [count=2000]   create/hand off/destroy shared textures,
//                                        the ledger's slots must not leak
//   d3d11_share.exe ledger               print the ledger's counters
//
//   d3d11_share.exe fmt <all|a8|r8|r8g8|r16|r16g16|b5g6r5|r10g10b10a2|rgba16f|
//                        yuy2|nv12|p010|p016|bgra8> [kmt|nt] [w h]
//                                        one shared texture per format, every
//                                        byte of every plane checked both ways
//                                        (docs/shared-formats.md; fmt_share)
//
// `perf` skips the readbacks (no correctness check) and reports the
// producer's flush+release time and hand-offs per second.
//
// keyed-load checks keyed-mutex ordering under load (see keyed_load below).
//
//   A (this process): a shared B8G8R8A8 render-target texture, filled with a
//     coordinate pattern (UpdateSubresource, then a clear of a 64x64 corner
//     through a render-target view), shared as
//       nt        an NT handle by name (CreateSharedHandle, OpenSharedResourceByName)
//       nt-keyed  the same with a keyed mutex (A releases key 1, B 2)
//       kmt       a legacy DXGI shared handle (GetSharedHandle, OpenSharedResource)
//   B (a second process, started by A): opens it, reads it back (staging copy)
//     and checks the pattern and the corner, writes a second pattern into the
//     lower half, flushes (and releases key 2), exits.
//   A: reads the lower half back: B's writes are in A's texture.
//
// HELIOS_ICD=nvk / venus selects the UMD backend per process (both inherit it).
// Exit code 0 when everything matched. Run in the interactive session
// (tools/run-in-session.ps1) or over SSH; no window is needed.
//
// Build (MinGW-w64 on Linux):
//   x86_64-w64-mingw32-g++ -O2 -static d3d11_share.cpp -o d3d11_share.exe \
//       -ld3d11 -ldxgi -luuid -ladvapi32
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11_1.h>
#include <dxgi1_2.h>

#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <cwchar>

static const wchar_t kName[] = L"Local\\helios_d3d11_share";
static int failed;
static const char *role = "A";

static void check(const char *what, bool ok) {
  std::printf("[%s] %s: %s\n", ok ? " ok " : "FAIL", role, what);
  std::fflush(stdout);
  if (!ok) failed = 1;
}

static void print_ledger(const char *when);

static UINT32 texel(UINT x, UINT y, UINT32 seed) {
  return ((x * 7919u) ^ (y * 104729u) ^ seed) | 0xff000000u;
}

static IDXGIAdapter1 *helios_adapter() {
  IDXGIFactory1 *f = nullptr;
  if (FAILED(CreateDXGIFactory1(__uuidof(IDXGIFactory1), reinterpret_cast<void **>(&f)))) return nullptr;
  IDXGIAdapter1 *a = nullptr, *pick = nullptr;
  for (UINT i = 0; f->EnumAdapters1(i, &a) != DXGI_ERROR_NOT_FOUND; ++i) {
    DXGI_ADAPTER_DESC1 d{};
    a->GetDesc1(&d);
    std::printf("       %s: adapter %u: %ls (vendor 0x%04x)\n", role, i, d.Description, d.VendorId);
    if (!pick && (std::wcsstr(d.Description, L"Helios") || std::wcsstr(d.Description, L"Conduit"))) {
      pick = a;
    } else {
      a->Release();
    }
  }
  f->Release();
  return pick;
}

static bool make_device(ID3D11Device1 **dev, ID3D11DeviceContext **ctx) {
  // The Helios adapter by name, else the default hardware adapter.
  IDXGIAdapter1 *a = helios_adapter();
  ID3D11Device *d = nullptr;
  D3D_FEATURE_LEVEL fl = D3D_FEATURE_LEVEL_11_0;
  HRESULT hr = D3D11CreateDevice(a, a ? D3D_DRIVER_TYPE_UNKNOWN : D3D_DRIVER_TYPE_HARDWARE, nullptr,
                                 0, &fl, 1, D3D11_SDK_VERSION, &d, nullptr, ctx);
  if (a) a->Release();
  if (FAILED(hr)) {
    std::printf("       %s: D3D11CreateDevice 0x%08lx\n", role, static_cast<unsigned long>(hr));
    return false;
  }
  hr = d->QueryInterface(__uuidof(ID3D11Device1), reinterpret_cast<void **>(dev));
  d->Release();
  return SUCCEEDED(hr);
}

// Copy `tex` to a staging texture and count texels of rows y0..y1 that differ
// from `seed`'s pattern (and, with corner, the 64x64 clear colour at 0,0).
static long count_bad(ID3D11Device1 *dev, ID3D11DeviceContext *ctx, ID3D11Texture2D *tex, UINT w,
                      UINT y0, UINT y1, UINT32 seed, bool corner) {
  D3D11_TEXTURE2D_DESC d{};
  tex->GetDesc(&d);
  d.Usage = D3D11_USAGE_STAGING;
  d.BindFlags = 0;
  d.MiscFlags = 0;
  d.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
  ID3D11Texture2D *st = nullptr;
  if (FAILED(dev->CreateTexture2D(&d, nullptr, &st))) return -1;
  ctx->CopyResource(st, tex);
  D3D11_MAPPED_SUBRESOURCE m{};
  if (FAILED(ctx->Map(st, 0, D3D11_MAP_READ, 0, &m))) {
    st->Release();
    return -1;
  }
  long bad = 0;
  for (UINT y = y0; y < y1; y++) {
    const UINT32 *row = reinterpret_cast<const UINT32 *>(static_cast<const BYTE *>(m.pData) + y * m.RowPitch);
    for (UINT x = 0; x < w; x++) {
      const UINT32 want = corner && x < 64 && y < 64 ? 0xff00ff00u : texel(x, y, seed);
      bad += row[x] != want;
    }
  }
  ctx->Unmap(st, 0);
  st->Release();
  return bad;
}

static void write_rows(ID3D11DeviceContext *ctx, ID3D11Texture2D *tex, UINT w, UINT y0, UINT y1, UINT32 seed) {
  UINT32 *buf = static_cast<UINT32 *>(std::malloc(size_t(w) * (y1 - y0) * 4));
  for (UINT y = y0; y < y1; y++)
    for (UINT x = 0; x < w; x++) buf[size_t(y - y0) * w + x] = texel(x, y, seed);
  D3D11_BOX box{0, y0, 0, w, y1, 1};
  ctx->UpdateSubresource(tex, 0, &box, buf, w * 4, 0);
  std::free(buf);
}

static int opener(const char *mode, UINT w, UINT h, const char *kmt) {
  role = "B";
  ID3D11Device1 *dev = nullptr;
  ID3D11DeviceContext *ctx = nullptr;
  if (!make_device(&dev, &ctx)) {
    check("a D3D11 device", false);
    return 1;
  }
  ID3D11Texture2D *tex = nullptr;
  HRESULT hr;
  if (!std::strcmp(mode, "kmt")) {
    HANDLE hs = reinterpret_cast<HANDLE>(static_cast<uintptr_t>(std::strtoull(kmt, nullptr, 0)));
    hr = dev->OpenSharedResource(hs, __uuidof(ID3D11Texture2D), reinterpret_cast<void **>(&tex));
  } else {
    hr = dev->OpenSharedResourceByName(kName, DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE,
                                       __uuidof(ID3D11Texture2D), reinterpret_cast<void **>(&tex));
  }
  std::printf("       B: open (%s): 0x%08lx\n", mode, static_cast<unsigned long>(hr));
  check("open A's shared texture", SUCCEEDED(hr) && tex);
  if (FAILED(hr) || !tex) return 1;

  IDXGIKeyedMutex *km = nullptr;
  const bool keyed = !std::strcmp(mode, "nt-keyed");
  if (keyed) {
    tex->QueryInterface(__uuidof(IDXGIKeyedMutex), reinterpret_cast<void **>(&km));
    hr = km ? km->AcquireSync(1, 10000) : E_NOINTERFACE;
    check("AcquireSync(1)", hr == S_OK);
  }
  long bad = count_bad(dev, ctx, tex, w, 0, h, 0x00a5c3e1u, true);
  std::printf("       B: %ld of %u texels differ\n", bad, w * h);
  check("A's texels, corner clear included", bad == 0);
  write_rows(ctx, tex, w, h / 2, h, 0x003c5a77u);
  ctx->Flush();
  if (keyed) check("ReleaseSync(2)", km->ReleaseSync(2) == S_OK);
  else {
    // No keyed mutex: wait for B's GPU work before telling A (an event query)
    D3D11_QUERY_DESC qd{D3D11_QUERY_EVENT, 0};
    ID3D11Query *q = nullptr;
    dev->CreateQuery(&qd, &q);
    ctx->End(q);
    BOOL done = FALSE;
    while (ctx->GetData(q, &done, sizeof(done), 0) != S_OK || !done) Sleep(1);
    q->Release();
  }
  if (km) km->Release();
  tex->Release();
  ctx->Release();
  dev->Release();
  std::printf("%s\n", failed ? "B FAILED" : "B PASSED");
  return failed;
}

// ---- keyed-load: ordering under load -------------------------------------
//
// B opens the texture first and blocks in AcquireSync(1). Each round, A holds
// the key, queues `load` full-texture copies (junk and pattern alternately,
// the pattern last), and releases key 1 at once; B, already waiting, reads
// right away and must see round r's pattern, then writes its own pattern into
// the lower half and releases key 2, which A acquires and checks. A driver
// whose GPU work is not complete when the key is released (NVK without the
// UMD's keyed-mutex flush wait) shows up as wrong texels in B.

static UINT32 round_seed(UINT r, bool b) { return 0x1000193u * (r + 1) ^ (b ? 0x5bd1e995u : 0x2545f491u); }

// The KMD's flush-gate counters (docs/flush-gate.md section 6), DWORDs under
// HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render. Published on the
// present edge (DWM's presents refresh them). Read only.
static const char *const kFlushGateCounters[] = {"FlGRec", "FlGStrm", "FlGFnc", "FlGWire", "FlGDeg"};
static const int kFlushGateCounterCount = 5;

static void read_flush_gate_counters(DWORD *out) {
  for (int i = 0; i < kFlushGateCounterCount; i++) {
    DWORD v = 0, size = sizeof(v);
    if (RegGetValueA(HKEY_LOCAL_MACHINE, "SYSTEM\\CurrentControlSet\\Services\\helios_kmd_render",
                     kFlushGateCounters[i], RRF_RT_REG_DWORD, nullptr, &v, &size) != ERROR_SUCCESS)
      v = 0xffffffffu;
    out[i] = v;
  }
}

static void print_flush_gate_counters(const DWORD *before, const DWORD *after) {
  std::printf("       A: KMD flush gate:");
  for (int i = 0; i < kFlushGateCounterCount; i++) {
    if (after[i] == 0xffffffffu)
      std::printf(" %s=absent", kFlushGateCounters[i]);
    else
      std::printf(" %s=%lu (+%lu)", kFlushGateCounters[i], static_cast<unsigned long>(after[i]),
                  static_cast<unsigned long>(after[i] - (before[i] == 0xffffffffu ? 0 : before[i])));
  }
  std::printf("\n");
}

static ID3D11Texture2D *make_plain(ID3D11Device1 *dev, UINT w, UINT h) {
  D3D11_TEXTURE2D_DESC d{};
  d.Width = w;
  d.Height = h;
  d.MipLevels = 1;
  d.ArraySize = 1;
  d.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
  d.SampleDesc.Count = 1;
  d.Usage = D3D11_USAGE_DEFAULT;
  d.BindFlags = D3D11_BIND_SHADER_RESOURCE;
  ID3D11Texture2D *t = nullptr;
  dev->CreateTexture2D(&d, nullptr, &t);
  return t;
}

static int keyed_load_opener(UINT w, UINT h, UINT rounds, bool perf) {
  role = "B";
  ID3D11Device1 *dev = nullptr;
  ID3D11DeviceContext *ctx = nullptr;
  if (!make_device(&dev, &ctx)) {
    check("a D3D11 device", false);
    return 1;
  }
  ID3D11Texture2D *tex = nullptr;
  HRESULT hr = dev->OpenSharedResourceByName(kName, DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE,
                                             __uuidof(ID3D11Texture2D), reinterpret_cast<void **>(&tex));
  check("open A's shared texture", SUCCEEDED(hr) && tex);
  if (FAILED(hr) || !tex) return 1;
  IDXGIKeyedMutex *km = nullptr;
  tex->QueryInterface(__uuidof(IDXGIKeyedMutex), reinterpret_cast<void **>(&km));
  long worst = 0;
  UINT bad_rounds = 0;
  for (UINT r = 0; r < rounds; r++) {
    if (km->AcquireSync(1, 30000) != S_OK) {
      check("AcquireSync(1)", false);
      return 1;
    }
    long bad = perf ? 0 : count_bad(dev, ctx, tex, w, 0, h, round_seed(r, false), false);
    if (bad) {
      bad_rounds++;
      if (bad > worst) worst = bad;
      std::printf("       B: round %u: %ld texels are not A's round-%u pattern\n", r, bad, r);
    }
    write_rows(ctx, tex, w, h / 2, h, round_seed(r, true));
    ctx->Flush();
    km->ReleaseSync(2);
  }
  std::printf("       B: %u of %u rounds wrong (worst %ld texels)\n", bad_rounds, rounds, worst);
  check("every round, B saw all of A's work at its AcquireSync", bad_rounds == 0);
  km->Release();
  tex->Release();
  ctx->Release();
  dev->Release();
  std::printf("%s\n", failed ? "B FAILED" : "B PASSED");
  return failed;
}

static int keyed_load(UINT w, UINT h, UINT rounds, UINT load, bool perf) {
  ID3D11Device1 *dev = nullptr;
  ID3D11DeviceContext *ctx = nullptr;
  if (!make_device(&dev, &ctx)) {
    check("a D3D11 device", false);
    return 1;
  }
  D3D11_TEXTURE2D_DESC d{};
  d.Width = w;
  d.Height = h;
  d.MipLevels = 1;
  d.ArraySize = 1;
  d.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
  d.SampleDesc.Count = 1;
  d.Usage = D3D11_USAGE_DEFAULT;
  d.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
  d.MiscFlags = D3D11_RESOURCE_MISC_SHARED_NTHANDLE | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX;
  ID3D11Texture2D *tex = nullptr;
  check("a keyed-mutex shared texture", SUCCEEDED(dev->CreateTexture2D(&d, nullptr, &tex)));
  ID3D11Texture2D *junk = make_plain(dev, w, h), *pat = make_plain(dev, w, h);
  if (!tex || !junk || !pat) return 1;
  write_rows(ctx, junk, w, 0, h, 0xdeadbeefu);
  IDXGIKeyedMutex *km = nullptr;
  tex->QueryInterface(__uuidof(IDXGIKeyedMutex), reinterpret_cast<void **>(&km));
  check("AcquireSync(0)", km->AcquireSync(0, 10000) == S_OK);
  IDXGIResource1 *res = nullptr;
  HANDLE nt = nullptr;
  tex->QueryInterface(__uuidof(IDXGIResource1), reinterpret_cast<void **>(&res));
  check("CreateSharedHandle by name",
        res && SUCCEEDED(res->CreateSharedHandle(nullptr, DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE,
                                                 kName, &nt)));
  if (res) res->Release();

  char self[MAX_PATH], cmd[1024];
  GetModuleFileNameA(nullptr, self, sizeof(self));
  std::snprintf(cmd, sizeof(cmd), "\"%s\" open keyed-load %u %u %u %u", self, w, h, rounds,
                perf ? 1u : 0u);
  STARTUPINFOA si{};
  si.cb = sizeof(si);
  PROCESS_INFORMATION pi{};
  if (!CreateProcessA(nullptr, cmd, nullptr, nullptr, FALSE, 0, nullptr, nullptr, &si, &pi)) {
    check("start B", false);
    return 1;
  }
  Sleep(1500); // B opens and blocks in AcquireSync(1)
  DWORD counters_before[kFlushGateCounterCount];
  read_flush_gate_counters(counters_before);

  UINT bad_rounds = 0;
  LARGE_INTEGER f, t0, t1, loop0, loop1;
  QueryPerformanceFrequency(&f);
  double release_ms = 0;
  QueryPerformanceCounter(&loop0);
  for (UINT r = 0; r < rounds; r++) {
    write_rows(ctx, pat, w, 0, h, round_seed(r, false));
    for (UINT i = 0; i < load; i++) ctx->CopyResource(tex, (i & 1) ? pat : junk);
    ctx->CopyResource(tex, pat);
    // The producer's cost of a hand-off: flush (where a releaser-side CPU
    // wait would drain the GPU) and the release.
    QueryPerformanceCounter(&t0);
    ctx->Flush();
    km->ReleaseSync(1);
    QueryPerformanceCounter(&t1);
    release_ms += 1000.0 * double(t1.QuadPart - t0.QuadPart) / double(f.QuadPart);
    if (km->AcquireSync(2, 30000) != S_OK) {
      check("AcquireSync(2)", false);
      break;
    }
    if (!perf && count_bad(dev, ctx, tex, w, h / 2, h, round_seed(r, true), false)) bad_rounds++;
  }
  QueryPerformanceCounter(&loop1);
  const double loop_s = double(loop1.QuadPart - loop0.QuadPart) / double(f.QuadPart);
  std::printf("       A: %u rounds of %u copies, flush+ReleaseSync took %.3f ms on average, "
              "%.0f hand-offs/s (2 per round)%s\n", rounds, load, release_ms / rounds,
              2.0 * rounds / loop_s, perf ? ", perf mode: no readbacks" : "");
  check("every round, A saw B's writes at its AcquireSync", bad_rounds == 0);
  Sleep(500); // the KMD publishes on the next present edge (DWM)
  DWORD counters_after[kFlushGateCounterCount];
  read_flush_gate_counters(counters_after);
  print_flush_gate_counters(counters_before, counters_after);
  print_ledger("end");
  WaitForSingleObject(pi.hProcess, 120000);
  DWORD code = 1;
  GetExitCodeProcess(pi.hProcess, &code);
  CloseHandle(pi.hProcess);
  CloseHandle(pi.hThread);
  check("B passed", code == 0);
  km->ReleaseSync(0);
  CloseHandle(nt);
  km->Release();
  junk->Release();
  pat->Release();
  tex->Release();
  ctx->Release();
  dev->Release();
  std::printf("%s\n", failed ? "SHARE FAILED" : "SHARE PASSED");
  return failed;
}

// The UMD's hand-off ledger header (umd/bridge/dxvk_bridge.cpp helios_handoff):
// records and slots in use, fallbacks to the CPU wait, sweeps, hand-offs.
static bool read_ledger(unsigned *out) {
  HANDLE m = OpenFileMappingW(FILE_MAP_READ, FALSE, L"Local\\HeliosHandoffLedger4");
  if (!m)
    return false;
  const unsigned *v = static_cast<const unsigned *>(MapViewOfFile(m, FILE_MAP_READ, 0, 0, 32));
  CloseHandle(m);
  if (!v)
    return false;
  for (int i = 0; i < 8; i++)
    out[i] = v[i];
  UnmapViewOfFile(v);
  return out[0] == 0x344C4448u;
}

static void print_ledger(const char *when) {
  unsigned l[8];
  if (!read_ledger(l)) {
    std::printf("       ledger (%s): absent\n", when);
    return;
  }
  std::printf("       ledger (%s): %u records, %u slots in use, %u fallbacks, %u sweeps, %u hand-offs\n",
              when, l[3], l[4], l[5], l[6], l[7]);
}

// churn: create, hand off and destroy `count` shared textures; the ledger's
// slots in use must come back to where they started.
static int churn(UINT count) {
  ID3D11Device1 *dev = nullptr;
  ID3D11DeviceContext *ctx = nullptr;
  if (!make_device(&dev, &ctx)) {
    check("a D3D11 device", false);
    return 1;
  }
  print_ledger("before");
  unsigned before[8] = {}, after[8] = {};
  read_ledger(before);
  for (UINT i = 0; i < count; i++) {
    D3D11_TEXTURE2D_DESC d{};
    d.Width = 256;
    d.Height = 256;
    d.MipLevels = 1;
    d.ArraySize = 1;
    d.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
    d.SampleDesc.Count = 1;
    d.Usage = D3D11_USAGE_DEFAULT;
    d.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
    d.MiscFlags = D3D11_RESOURCE_MISC_SHARED;
    ID3D11Texture2D *t = nullptr;
    if (FAILED(dev->CreateTexture2D(&d, nullptr, &t)) || !t) {
      check("a shared texture", false);
      return 1;
    }
    ID3D11RenderTargetView *rtv = nullptr;
    dev->CreateRenderTargetView(t, nullptr, &rtv);
    const float c[4] = {float(i & 1), 0, 0, 1};
    if (rtv) ctx->ClearRenderTargetView(rtv, c);
    ctx->Flush(); // a hand-off: publishes on the texture
    if (rtv) rtv->Release();
    t->Release();
    if (i % 64 == 63) {
      ctx->Flush();
      print_ledger("during");
    }
  }
  ctx->ClearState();
  ctx->Flush();
  Sleep(200);
  read_ledger(after);
  print_ledger("after");
  check("slots in use came back (no leak per texture)", after[4] <= before[4] + 1);
  ctx->Release();
  dev->Release();
  std::printf("%s\n", failed ? "CHURN FAILED" : "CHURN PASSED");
  return failed;
}

// ---- fmt: one shared texture of each format the desktop and browsers share --
//
// guest/windows/docs/shared-formats.md. A creates a shared texture of the
// format, fills every byte of every plane with a pattern (UpdateSubresource),
// waits for the GPU and starts B; B opens it, reads every byte back through a
// staging copy, writes a second pattern over the whole texture and waits for
// its GPU work; A reads B's pattern back. Bytes, not texels: pixel-exact for
// every format, planes included (D3D11 lays a planar subresource out as plane
// 0's rows, then plane 1's rows, at one row pitch).

struct FmtSpec {
  const char *name;
  DXGI_FORMAT format;
  UINT bpp0;     // bytes per texel unit of plane 0 (YUY2: per 2 pixels)
  UINT hdiv0;    // plane 0 texel units per row = ceil(w / hdiv0)
  bool two_planes; // 4:2:0, plane 1: ceil(w/2) pairs of 2*bpp0 bytes, ceil(h/2) rows
  UINT bind;
};

static const FmtSpec kFmts[] = {
    {"bgra8", DXGI_FORMAT_B8G8R8A8_UNORM, 4, 1, false, D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET},
    {"a8", DXGI_FORMAT_A8_UNORM, 1, 1, false, D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET},
    {"r8", DXGI_FORMAT_R8_UNORM, 1, 1, false, D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET},
    {"r8g8", DXGI_FORMAT_R8G8_UNORM, 2, 1, false, D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET},
    {"r16", DXGI_FORMAT_R16_UNORM, 2, 1, false, D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET},
    {"r16g16", DXGI_FORMAT_R16G16_UNORM, 4, 1, false, D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET},
    {"b5g6r5", DXGI_FORMAT_B5G6R5_UNORM, 2, 1, false, D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET},
    {"r10g10b10a2", DXGI_FORMAT_R10G10B10A2_UNORM, 4, 1, false, D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET},
    {"rgba16f", DXGI_FORMAT_R16G16B16A16_FLOAT, 8, 1, false, D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET},
    {"yuy2", DXGI_FORMAT_YUY2, 4, 2, false, D3D11_BIND_SHADER_RESOURCE},
    {"nv12", DXGI_FORMAT_NV12, 1, 1, true, D3D11_BIND_SHADER_RESOURCE},
    {"p010", DXGI_FORMAT_P010, 2, 1, true, D3D11_BIND_SHADER_RESOURCE},
    {"p016", DXGI_FORMAT_P016, 2, 1, true, D3D11_BIND_SHADER_RESOURCE},
};

static const FmtSpec *find_fmt(const char *name) {
  for (const FmtSpec &f : kFmts)
    if (!std::strcmp(f.name, name)) return &f;
  return nullptr;
}

// Rows of the whole subresource and bytes of row `r` (plane 0 rows first).
static UINT fmt_rows(const FmtSpec &f, UINT h) { return h + (f.two_planes ? (h + 1) / 2 : 0); }
static UINT fmt_row_bytes(const FmtSpec &f, UINT w, UINT h, UINT r) {
  if (r < h) return ((w + f.hdiv0 - 1) / f.hdiv0) * f.bpp0;
  return ((w + 1) / 2) * f.bpp0 * 2;
}

static BYTE fmt_byte(UINT r, UINT x, UINT32 seed) {
  UINT32 v = (r * 2654435761u) ^ (x * 40503u) ^ seed;
  v ^= v >> 13;
  v *= 0x5bd1e995u;
  return BYTE(v ^ (v >> 15));
}

static void fmt_fill(ID3D11DeviceContext *ctx, ID3D11Texture2D *tex, const FmtSpec &f, UINT w, UINT h,
                     UINT32 seed) {
  const UINT pitch = (fmt_row_bytes(f, w, h, 0) > fmt_row_bytes(f, w, h, h) ? fmt_row_bytes(f, w, h, 0)
                                                                           : fmt_row_bytes(f, w, h, h)) +
                     64;
  const UINT rows = fmt_rows(f, h);
  BYTE *buf = static_cast<BYTE *>(std::calloc(size_t(pitch) * rows, 1));
  for (UINT r = 0; r < rows; r++)
    for (UINT x = 0, n = fmt_row_bytes(f, w, h, r); x < n; x++) buf[size_t(r) * pitch + x] = fmt_byte(r, x, seed);
  ctx->UpdateSubresource(tex, 0, nullptr, buf, pitch, pitch * rows);
  std::free(buf);
}

// Bytes of `tex` that differ from `seed`'s pattern, -1 when it cannot be read.
static long fmt_count_bad(ID3D11Device1 *dev, ID3D11DeviceContext *ctx, ID3D11Texture2D *tex, const FmtSpec &f,
                          UINT w, UINT h, UINT32 seed) {
  D3D11_TEXTURE2D_DESC d{};
  tex->GetDesc(&d);
  d.Usage = D3D11_USAGE_STAGING;
  d.BindFlags = 0;
  d.MiscFlags = 0;
  d.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
  ID3D11Texture2D *st = nullptr;
  if (FAILED(dev->CreateTexture2D(&d, nullptr, &st))) return -1;
  ctx->CopyResource(st, tex);
  D3D11_MAPPED_SUBRESOURCE m{};
  if (FAILED(ctx->Map(st, 0, D3D11_MAP_READ, 0, &m))) {
    st->Release();
    return -1;
  }
  long bad = 0, shown = 0;
  for (UINT r = 0, rows = fmt_rows(f, h); r < rows; r++) {
    const BYTE *row = static_cast<const BYTE *>(m.pData) + size_t(r) * m.RowPitch;
    for (UINT x = 0, n = fmt_row_bytes(f, w, h, r); x < n; x++) {
      if (row[x] != fmt_byte(r, x, seed)) {
        if (shown++ < 4)
          std::printf("       %s: row %u byte %u: 0x%02x, want 0x%02x\n", role, r, x, row[x], fmt_byte(r, x, seed));
        bad++;
      }
    }
  }
  ctx->Unmap(st, 0);
  st->Release();
  return bad;
}

static void wait_gpu(ID3D11Device1 *dev, ID3D11DeviceContext *ctx) {
  D3D11_QUERY_DESC qd{D3D11_QUERY_EVENT, 0};
  ID3D11Query *q = nullptr;
  dev->CreateQuery(&qd, &q);
  ctx->End(q);
  BOOL done = FALSE;
  while (ctx->GetData(q, &done, sizeof(done), 0) != S_OK || !done) Sleep(1);
  q->Release();
}

static const UINT32 kFmtSeedA = 0x1b873593u, kFmtSeedB = 0xcc9e2d51u;

static int fmt_opener(const FmtSpec &f, const char *mode, UINT w, UINT h, const char *kmt) {
  role = "B";
  ID3D11Device1 *dev = nullptr;
  ID3D11DeviceContext *ctx = nullptr;
  if (!make_device(&dev, &ctx)) {
    check("a D3D11 device", false);
    return 1;
  }
  ID3D11Texture2D *tex = nullptr;
  HRESULT hr;
  if (!std::strcmp(mode, "kmt")) {
    HANDLE hs = reinterpret_cast<HANDLE>(static_cast<uintptr_t>(std::strtoull(kmt, nullptr, 0)));
    hr = dev->OpenSharedResource(hs, __uuidof(ID3D11Texture2D), reinterpret_cast<void **>(&tex));
  } else {
    hr = dev->OpenSharedResourceByName(kName, DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE,
                                       __uuidof(ID3D11Texture2D), reinterpret_cast<void **>(&tex));
  }
  std::printf("       B: open %s (%s): 0x%08lx\n", f.name, mode, static_cast<unsigned long>(hr));
  check("open A's shared texture", SUCCEEDED(hr) && tex);
  if (FAILED(hr) || !tex) return 1;
  D3D11_TEXTURE2D_DESC d{};
  tex->GetDesc(&d);
  check("the opened texture has A's format and size", d.Format == f.format && d.Width == w && d.Height == h);
  long bad = fmt_count_bad(dev, ctx, tex, f, w, h, kFmtSeedA);
  std::printf("       B: %ld bytes differ from A's pattern\n", bad);
  check("A's bytes, every plane", bad == 0);
  fmt_fill(ctx, tex, f, w, h, kFmtSeedB);
  ctx->Flush();
  wait_gpu(dev, ctx);
  tex->Release();
  ctx->Release();
  dev->Release();
  std::printf("%s\n", failed ? "B FAILED" : "B PASSED");
  return failed;
}

static int fmt_share(const FmtSpec &f, const char *mode, UINT w, UINT h) {
  const bool kmt = !std::strcmp(mode, "kmt");
  const char *icd = std::getenv("HELIOS_ICD");
  std::printf("       A: fmt %s (DXGI %u), %s, %ux%u, HELIOS_ICD=%s\n", f.name, unsigned(f.format), mode, w, h,
              icd ? icd : "(unset)");
  ID3D11Device1 *dev = nullptr;
  ID3D11DeviceContext *ctx = nullptr;
  if (!make_device(&dev, &ctx)) {
    check("a D3D11 device", false);
    return 1;
  }
  UINT support = 0;
  dev->CheckFormatSupport(f.format, &support);
  std::printf("       A: format support 0x%08x\n", support);
  D3D11_TEXTURE2D_DESC d{};
  d.Width = w;
  d.Height = h;
  d.MipLevels = 1;
  d.ArraySize = 1;
  d.Format = f.format;
  d.SampleDesc.Count = 1;
  d.Usage = D3D11_USAGE_DEFAULT;
  d.BindFlags = f.bind;
  d.MiscFlags = kmt ? D3D11_RESOURCE_MISC_SHARED : D3D11_RESOURCE_MISC_SHARED_NTHANDLE | D3D11_RESOURCE_MISC_SHARED;
  ID3D11Texture2D *tex = nullptr;
  HRESULT hr = dev->CreateTexture2D(&d, nullptr, &tex);
  std::printf("       A: CreateTexture2D bind 0x%x misc 0x%x: 0x%08lx\n", d.BindFlags, d.MiscFlags,
              static_cast<unsigned long>(hr));
  check("a shared texture", SUCCEEDED(hr) && tex);
  if (FAILED(hr) || !tex) return 1;
  fmt_fill(ctx, tex, f, w, h, kFmtSeedA);
  long bad = fmt_count_bad(dev, ctx, tex, f, w, h, kFmtSeedA);
  check("A reads its own bytes", bad == 0);

  char kmtarg[32] = "0";
  HANDLE nt = nullptr;
  if (kmt) {
    IDXGIResource *res = nullptr;
    HANDLE hs = nullptr;
    tex->QueryInterface(__uuidof(IDXGIResource), reinterpret_cast<void **>(&res));
    hr = res ? res->GetSharedHandle(&hs) : E_NOINTERFACE;
    check("GetSharedHandle", SUCCEEDED(hr) && hs);
    std::snprintf(kmtarg, sizeof(kmtarg), "0x%llx", static_cast<unsigned long long>(reinterpret_cast<uintptr_t>(hs)));
    if (res) res->Release();
  } else {
    IDXGIResource1 *res = nullptr;
    tex->QueryInterface(__uuidof(IDXGIResource1), reinterpret_cast<void **>(&res));
    hr = res ? res->CreateSharedHandle(nullptr, DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE, kName, &nt)
             : E_NOINTERFACE;
    check("CreateSharedHandle by name", SUCCEEDED(hr) && nt);
    if (res) res->Release();
  }
  ctx->Flush();
  wait_gpu(dev, ctx);

  char self[MAX_PATH], cmd[1024];
  GetModuleFileNameA(nullptr, self, sizeof(self));
  std::snprintf(cmd, sizeof(cmd), "\"%s\" open-fmt %s %s %u %u %s", self, f.name, mode, w, h, kmtarg);
  STARTUPINFOA si{};
  si.cb = sizeof(si);
  PROCESS_INFORMATION pi{};
  if (!CreateProcessA(nullptr, cmd, nullptr, nullptr, FALSE, 0, nullptr, nullptr, &si, &pi)) {
    check("start B", false);
    return 1;
  }
  WaitForSingleObject(pi.hProcess, 120000);
  DWORD code = 1;
  GetExitCodeProcess(pi.hProcess, &code);
  CloseHandle(pi.hProcess);
  CloseHandle(pi.hThread);
  check("B passed", code == 0);

  bad = fmt_count_bad(dev, ctx, tex, f, w, h, kFmtSeedB);
  std::printf("       A: %ld bytes differ from B's pattern\n", bad);
  check("A sees B's bytes (the same memory)", bad == 0);
  if (nt) CloseHandle(nt);
  tex->Release();
  ctx->Release();
  dev->Release();
  std::printf("%s %s\n", f.name, failed ? "FMT FAILED" : "FMT PASSED");
  return failed;
}

int main(int argc, char **argv) {
  setvbuf(stdout, nullptr, _IONBF, 0);
  if (argc >= 7 && !std::strcmp(argv[1], "open-fmt")) {
    const FmtSpec *f = find_fmt(argv[2]);
    return f ? fmt_opener(*f, argv[3], UINT(std::atoi(argv[4])), UINT(std::atoi(argv[5])), argv[6]) : 2;
  }
  if (argc >= 3 && !std::strcmp(argv[1], "fmt")) {
    const char *mode = argc > 3 ? argv[3] : "kmt";
    const UINT w = argc > 5 ? UINT(std::atoi(argv[4])) : 1920;
    const UINT h = argc > 5 ? UINT(std::atoi(argv[5])) : 1080;
    if (!std::strcmp(argv[2], "all")) {
      int fails = 0;
      for (const FmtSpec &f : kFmts) {
        failed = 0;
        fails += fmt_share(f, mode, w, h) != 0;
      }
      std::printf("fmt all: %d format(s) failed\n", fails);
      return fails != 0;
    }
    const FmtSpec *f = find_fmt(argv[2]);
    if (!f || (std::strcmp(mode, "kmt") && std::strcmp(mode, "nt"))) {
      std::fprintf(stderr, "usage: %s fmt <all|bgra8|a8|r8|r8g8|r16|r16g16|b5g6r5|r10g10b10a2|rgba16f|yuy2|nv12|p010|p016> [kmt|nt] [w h]\n",
                   argv[0]);
      return 2;
    }
    return fmt_share(*f, mode, w, h);
  }
  if (argc >= 6 && !std::strcmp(argv[1], "open") && !std::strcmp(argv[2], "keyed-load"))
    return keyed_load_opener(UINT(std::atoi(argv[3])), UINT(std::atoi(argv[4])), UINT(std::atoi(argv[5])),
                             argc > 6 && std::atoi(argv[6]) != 0);
  if (argc >= 5 && !std::strcmp(argv[1], "open"))
    return opener(argv[2], UINT(std::atoi(argv[3])), UINT(std::atoi(argv[4])), argc > 5 ? argv[5] : "0");
  if (argc > 1 && !std::strcmp(argv[1], "churn"))
    return churn(argc > 2 ? UINT(std::atoi(argv[2])) : 2000);
  if (argc > 1 && !std::strcmp(argv[1], "ledger")) {
    print_ledger("now");
    return 0;
  }
  if (argc > 1 && !std::strcmp(argv[1], "keyed-load")) {
    std::printf("       A: mode keyed-load, HELIOS_ICD=%s\n", std::getenv("HELIOS_ICD") ? std::getenv("HELIOS_ICD") : "(unset)");
    return keyed_load(argc > 3 ? UINT(std::atoi(argv[2])) : 1920, argc > 3 ? UINT(std::atoi(argv[3])) : 1080,
                      argc > 4 ? UINT(std::atoi(argv[4])) : 20, argc > 5 ? UINT(std::atoi(argv[5])) : 400,
                      argc > 6 && !std::strcmp(argv[6], "perf"));
  }

  const char *mode = argc > 1 ? argv[1] : "nt";
  const UINT w = argc > 3 ? UINT(std::atoi(argv[2])) : 1920;
  const UINT h = argc > 3 ? UINT(std::atoi(argv[3])) : 1080;
  const bool kmt = !std::strcmp(mode, "kmt"), keyed = !std::strcmp(mode, "nt-keyed");
  if (!kmt && !keyed && std::strcmp(mode, "nt")) {
    std::fprintf(stderr, "usage: %s [nt|nt-keyed|kmt] [w h]\n", argv[0]);
    return 2;
  }
  const char *icd = std::getenv("HELIOS_ICD");
  std::printf("       A: mode %s, %ux%u, HELIOS_ICD=%s\n", mode, w, h, icd ? icd : "(unset)");

  ID3D11Device1 *dev = nullptr;
  ID3D11DeviceContext *ctx = nullptr;
  if (!make_device(&dev, &ctx)) {
    check("a D3D11 device", false);
    return 1;
  }
  D3D11_TEXTURE2D_DESC d{};
  d.Width = w;
  d.Height = h;
  d.MipLevels = 1;
  d.ArraySize = 1;
  d.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
  d.SampleDesc.Count = 1;
  d.Usage = D3D11_USAGE_DEFAULT;
  d.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
  d.MiscFlags = kmt ? D3D11_RESOURCE_MISC_SHARED
                    : D3D11_RESOURCE_MISC_SHARED_NTHANDLE |
                          (keyed ? D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX : D3D11_RESOURCE_MISC_SHARED);
  ID3D11Texture2D *tex = nullptr;
  HRESULT hr = dev->CreateTexture2D(&d, nullptr, &tex);
  std::printf("       A: CreateTexture2D misc 0x%x: 0x%08lx\n", d.MiscFlags, static_cast<unsigned long>(hr));
  check("a shared texture", SUCCEEDED(hr));
  if (FAILED(hr)) return 1;

  IDXGIKeyedMutex *km = nullptr;
  if (keyed) {
    tex->QueryInterface(__uuidof(IDXGIKeyedMutex), reinterpret_cast<void **>(&km));
    check("AcquireSync(0)", km && km->AcquireSync(0, 10000) == S_OK);
  }
  write_rows(ctx, tex, w, 0, h, 0x00a5c3e1u);
  ID3D11RenderTargetView *rtv = nullptr;
  dev->CreateRenderTargetView(tex, nullptr, &rtv);
  const float green[4] = {0, 1, 0, 1};
  D3D11_RECT r{0, 0, 64, 64};
  ID3D11DeviceContext1 *ctx1 = nullptr;
  ctx->QueryInterface(__uuidof(ID3D11DeviceContext1), reinterpret_cast<void **>(&ctx1));
  if (ctx1 && rtv) ctx1->ClearView(rtv, green, &r, 1);
  long bad = count_bad(dev, ctx, tex, w, 0, h, 0x00a5c3e1u, true);
  check("A reads its own texels", bad == 0);

  char kmtarg[32] = "0";
  HANDLE nt = nullptr;
  if (kmt) {
    IDXGIResource *res = nullptr;
    HANDLE hs = nullptr;
    tex->QueryInterface(__uuidof(IDXGIResource), reinterpret_cast<void **>(&res));
    hr = res ? res->GetSharedHandle(&hs) : E_NOINTERFACE;
    check("GetSharedHandle", SUCCEEDED(hr) && hs);
    std::snprintf(kmtarg, sizeof(kmtarg), "0x%llx", static_cast<unsigned long long>(reinterpret_cast<uintptr_t>(hs)));
    if (res) res->Release();
  } else {
    IDXGIResource1 *res = nullptr;
    tex->QueryInterface(__uuidof(IDXGIResource1), reinterpret_cast<void **>(&res));
    hr = res ? res->CreateSharedHandle(nullptr, DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE, kName, &nt)
             : E_NOINTERFACE;
    check("CreateSharedHandle by name", SUCCEEDED(hr) && nt);
    if (res) res->Release();
  }
  ctx->Flush();
  if (keyed) check("ReleaseSync(1)", km->ReleaseSync(1) == S_OK);
  else {
    D3D11_QUERY_DESC qd{D3D11_QUERY_EVENT, 0};
    ID3D11Query *q = nullptr;
    dev->CreateQuery(&qd, &q);
    ctx->End(q);
    BOOL done = FALSE;
    while (ctx->GetData(q, &done, sizeof(done), 0) != S_OK || !done) Sleep(1);
    q->Release();
  }

  char self[MAX_PATH], cmd[1024];
  GetModuleFileNameA(nullptr, self, sizeof(self));
  std::snprintf(cmd, sizeof(cmd), "\"%s\" open %s %u %u %s", self, mode, w, h, kmtarg);
  STARTUPINFOA si{};
  si.cb = sizeof(si);
  PROCESS_INFORMATION pi{};
  if (!CreateProcessA(nullptr, cmd, nullptr, nullptr, FALSE, 0, nullptr, nullptr, &si, &pi)) {
    check("start B", false);
    return 1;
  }
  WaitForSingleObject(pi.hProcess, 120000);
  DWORD code = 1;
  GetExitCodeProcess(pi.hProcess, &code);
  CloseHandle(pi.hProcess);
  CloseHandle(pi.hThread);
  check("B passed", code == 0);

  if (keyed) check("AcquireSync(2)", km->AcquireSync(2, 10000) == S_OK);
  long top = count_bad(dev, ctx, tex, w, 0, h / 2, 0x00a5c3e1u, true);
  long bottom = count_bad(dev, ctx, tex, w, h / 2, h, 0x003c5a77u, false);
  std::printf("       A: %ld texels differ above, %ld below\n", top, bottom);
  check("the upper half is still A's", top == 0);
  check("the lower half is what B wrote (the same memory)", bottom == 0);
  if (keyed) km->ReleaseSync(0);

  if (nt) CloseHandle(nt);
  if (km) km->Release();
  if (ctx1) ctx1->Release();
  if (rtv) rtv->Release();
  tex->Release();
  ctx->Release();
  dev->Release();
  std::printf("%s\n", failed ? "SHARE FAILED" : "SHARE PASSED");
  return failed;
}
