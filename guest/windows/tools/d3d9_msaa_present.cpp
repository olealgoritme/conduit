// d3d9_msaa_present.cpp - native Windows x86 D3D9 MSAA presentation regression probe.
//
// Direct3D 9 runs on D3D9On12 here. An MSAA back buffer reaches the D3D12 UMD's
// Present unresolved; the UMD gives each D3D9On12 MSAA render target a
// single-sample present image at creation and records a resolve into it on the
// Present command list (umd12 present12.rs, record_msaa_present). The probe
// renders a clear color plus one triangle, presents with D3DSWAPEFFECT_DISCARD,
// and checks two oracles separately: a GPU readback of the back buffer and the
// composed desktop pixels. A passing readback with failing screen pixels means
// correct rendering and a failed presentation; --present-only checks that the
// readback's synchronization is not hiding a presentation failure.
//
// Build (from the repository root, mingw-w64 i686):
//   i686-w64-mingw32-g++ -std=c++17 -O2 -static -Wall -Wextra
//       guest/windows/tools/d3d9_msaa_present.cpp -o d3d9_msaa_present.exe
//       -ld3d9 -lgdi32 -luser32
//
// Run in an interactive session with a visible, unoccluded window (the screen
// oracle samples the desktop with GetPixel; the window is made topmost):
//   d3d9_msaa_present.exe --samples 4 --reset-cycles 4
//   d3d9_msaa_present.exe --samples 4 --reset-cycles 4 --present-only
//   d3d9_msaa_present.exe --samples 4 --reset-cycles 4 --argb
// and the same with --samples 1 (single-sample control), 2 and 8.
//
// Usage: d3d9_msaa_present.exe [--samples 1|2|4|8] [--argb] [--reset-cycles 1..8]
//                              [--log PATH] [--present-only] [--no-screen-check]
//   --samples N       MSAA sample count (default 4); an unsupported windowed
//                     format/sample combination (CheckDeviceMultiSampleType) is SKIP.
//   --argb            D3DFMT_A8R8G8B8 instead of D3DFMT_X8R8G8B8; alpha (255) is
//                     then compared too, X8 ignores it.
//   --reset-cycles N  Reset the SAME device N times between phases, alternating the
//                     client/back buffer between 640x450 and 704x480 so the swap
//                     chain and the MSAA allocations are recreated (default 0: one
//                     80-frame phase; with resets every phase has 40 frames).
//   --log PATH        stdout to a file.
//   --present-only    no readback (GetBackBuffer/StretchRect/GetRenderTargetData);
//                     cannot be combined with --no-screen-check.
//   --no-screen-check no desktop GetPixel checks.
// With screen checks the probe sets HELIOS_NVK_PRESENT=2 (DWM composition rather
// than direct scanout); without them it keeps the caller's policy.
//
// Samples per phase: readback before Present at frames 0/30/50 (0/15/30 with
// resets) and screen after Present at 20/40/60 (10/25/35): the clear color at
// (4,4) and the triangle color at the center. Readback path: GetBackBuffer
// (MSAA) -> StretchRect(full size, filter NONE) -> single-sample RT ->
// GetRenderTargetData -> SYSTEMMEM; never GetRenderTargetData on the MSAA
// surface. Every default-pool surface and back-buffer reference of a phase is
// released before the next Reset. Failures are sticky across phases.
//
// Exit: 0 PASS, 1 API/setup failure (a failed Reset included), 2 oracle
// mismatch, incomplete sampling, WM_QUIT or fewer resets than requested, 77 SKIP.
// Not covered: exclusive fullscreen, device loss, multiple adapters, edge quality.
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d9.h>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <memory>
#include <utility>

namespace {
using UL = unsigned long;
constexpr int kInitWidth = 640, kInitHeight = 450;
constexpr int kAltWidth = 704, kAltHeight = 480;
constexpr unsigned kFrames = 80;                // legacy single-phase run
constexpr unsigned kResetPhaseFrames = 40;      // bounded per phase when resets are enabled
constexpr DWORD kClearColor = D3DCOLOR_ARGB(255, 5, 23, 41);
constexpr DWORD kTriangleColor = D3DCOLOR_ARGB(255, 32, 224, 64);
constexpr unsigned kReadbackFrameCount = 3, kScreenFrameCount = 3; // 0/30/50, 20/40/60
constexpr int kMaxResetCycles = 8;
constexpr int kMaxPhases = 1 + kMaxResetCycles;
constexpr const char* kFatalSummary =
    "SUMMARY gpu_readback=NOT_RUN composed_display=NOT_RUN api_failures=1 "
    "result=API_FAILURE exit=1\n";

struct Counters {
  int api_failures = 0, readback_checks = 0, readback_failures = 0;
  int display_checks = 0, display_failures = 0;
  int readback_setup_failures = 0;
};
template <class T> struct ComDeleter { void operator()(T* p) const { if (p) p->Release(); } };
template <class T> using Com = std::unique_ptr<T, ComDeleter<T>>;
struct WindowGuard {
  HWND hwnd = nullptr; const wchar_t* cls = nullptr;
  ~WindowGuard() {
    if (hwnd && IsWindow(hwnd)) DestroyWindow(hwnd);
    if (cls) UnregisterClassW(cls, GetModuleHandleW(nullptr));
  }
};
struct Vertex { float x, y, z, rhw; DWORD color; };

struct PhaseResult {
  int index = 0;
  bool is_reset = false;
  int width = 0, height = 0;
  Counters counts;
  bool reset_ok = true;
  HRESULT reset_hr = S_OK;
  bool complete = false;
  bool quit_seen = false;
};

struct PhaseSchedule {
  unsigned frames;
  unsigned readback_frames[kReadbackFrameCount];
  unsigned display_frames[kScreenFrameCount];
};

const char* ms_name(D3DMULTISAMPLE_TYPE t) {
  return t == D3DMULTISAMPLE_NONE ? "NONE" : t == D3DMULTISAMPLE_2_SAMPLES ? "2" :
         t == D3DMULTISAMPLE_4_SAMPLES ? "4" : t == D3DMULTISAMPLE_8_SAMPLES ? "8" : "other";
}
D3DMULTISAMPLE_TYPE ms_type_for(int s) {
  return s == 2 ? D3DMULTISAMPLE_2_SAMPLES : s == 4 ? D3DMULTISAMPLE_4_SAMPLES :
         s == 8 ? D3DMULTISAMPLE_8_SAMPLES : D3DMULTISAMPLE_NONE;
}
const char* format_name(D3DFORMAT f) {
  return f == D3DFMT_X8R8G8B8 ? "X8R8G8B8" : f == D3DFMT_A8R8G8B8 ? "A8R8G8B8" : "other";
}
DWORD pixel_mask(D3DFORMAT f) { return f == D3DFMT_A8R8G8B8 ? 0xFFFFFFFFu : 0x00FFFFFFu; }
bool parse_samples_arg(const char* s, int& out) {
  if (!s || *s < '0' || *s > '9') return false; // reject empty, signs, whitespace, junk
  char* end = nullptr;
  const long v = std::strtol(s, &end, 10);
  if (!end || *end != '\0' || v < 0 || v > 8) return false;
  out = static_cast<int>(v);
  return true;
}
bool parse_reset_arg(const char* s, int& out) {
  if (!s || *s < '0' || *s > '9') return false; // reject empty, signs, whitespace, junk
  char* end = nullptr;
  const long v = std::strtol(s, &end, 10);
  if (!end || *end != '\0' || v < 1 || v > kMaxResetCycles) return false;
  out = static_cast<int>(v);
  return true;
}
void log_hr(const char* what, HRESULT hr) {
  std::printf("  %s hr=0x%08lX\n", what, UL(hr));
}
int api_fail(const char* what, HRESULT hr) {
  std::printf("FAIL %s hr=0x%08lX\n", what, UL(hr));
  std::printf("%s", kFatalSummary);
  return 1;
}
int api_fail_msg(const char* what) {
  std::printf("FAIL %s\n", what);
  std::printf("%s", kFatalSummary);
  return 1;
}
bool api_step(HRESULT hr, const char* what, Counters& c) {
  if (SUCCEEDED(hr)) return true;
  std::printf("FAIL %s hr=0x%08lX\n", what, UL(hr));
  ++c.api_failures;
  return false;
}
void add_counts(Counters& dst, const Counters& src) {
  dst.api_failures += src.api_failures;
  dst.readback_checks += src.readback_checks;
  dst.readback_failures += src.readback_failures;
  dst.display_checks += src.display_checks;
  dst.display_failures += src.display_failures;
  dst.readback_setup_failures += src.readback_setup_failures;
}

LRESULT CALLBACK wnd_proc(HWND hwnd, UINT msg, WPARAM wp, LPARAM lp) {
  if (msg == WM_CLOSE) { DestroyWindow(hwnd); return 0; }
  if (msg == WM_DESTROY) { PostQuitMessage(0); return 0; }
  return DefWindowProcW(hwnd, msg, wp, lp);
}
bool pump_messages() {
  MSG msg;
  while (PeekMessageW(&msg, nullptr, 0, 0, PM_REMOVE)) {
    if (msg.message == WM_QUIT) return false;
    TranslateMessage(&msg); DispatchMessageW(&msg);
  }
  return true;
}
bool pump_for(int ms) {
  const DWORD start = GetTickCount();
  while (GetTickCount() - start < static_cast<DWORD>(ms)) {
    if (!pump_messages()) return false;
    Sleep(10);
  }
  return pump_messages();
}
void print_modules(const char* phase) {
  std::printf("-- modules (%s) process_bits=%u --\n", phase, unsigned(sizeof(void*) * 8));
  const char* names[] = {"d3d9.dll", "d3d9on12.dll", "helios_umd12_32.dll"};
  for (const char* name : names) {
    HMODULE mod = GetModuleHandleA(name);
    char path[MAX_PATH] = {};
    DWORD n = mod ? GetModuleFileNameA(mod, path, MAX_PATH) : 0;
    std::printf("module %-22s = %s\n", name,
        mod ? (n ? path : "<GetModuleFileNameA failed>") : "<not loaded>");
  }
}

void readback_frame(IDirect3DDevice9* dev, IDirect3DSurface9* resolve,
    IDirect3DSurface9* sys, D3DFORMAT format, D3DMULTISAMPLE_TYPE msaa,
    int w, int h, unsigned frame, Counters& c) {
  ++c.readback_checks;
  std::printf("frame=%u stage=pre-present gpu_readback=ON\n", frame);
  IDirect3DSurface9* raw = nullptr;
  HRESULT hr = dev->GetBackBuffer(0, 0, D3DBACKBUFFER_TYPE_MONO, &raw);
  log_hr("GetBackBuffer", hr);
  if (FAILED(hr) || !raw) { ++c.api_failures; ++c.readback_failures; return; }
  Com<IDirect3DSurface9> bb(raw);
  D3DSURFACE_DESC desc = {}; const HRESULT dhr = bb->GetDesc(&desc);
  std::printf("  backbuffer format=%s msaa=%s quality=%lu (GetDesc hr=0x%08lX)\n",
      format_name(desc.Format), ms_name(desc.MultiSampleType), UL(desc.MultiSampleQuality), UL(dhr));
  if (FAILED(dhr)) { ++c.api_failures; ++c.readback_failures; return; }
  if (desc.Width != UINT(w) || desc.Height != UINT(h) || desc.Format != format ||
      desc.MultiSampleType != msaa || desc.MultiSampleQuality != 0) {
    std::printf("  FAIL backbuffer description differs from requested %dx%d format=%s msaa=%s quality=0\n",
        w, h, format_name(format), ms_name(msaa));
    ++c.readback_failures; return;
  }
  hr = dev->StretchRect(bb.get(), nullptr, resolve, nullptr, D3DTEXF_NONE);
  log_hr("StretchRect(MSAA->single-sample, full-size, filter=NONE)", hr);
  if (FAILED(hr)) { ++c.api_failures; ++c.readback_failures; return; }
  hr = dev->GetRenderTargetData(resolve, sys);
  log_hr("GetRenderTargetData(single-sample RT->SYSTEMMEM)", hr);
  if (FAILED(hr)) { ++c.api_failures; ++c.readback_failures; return; }
  D3DLOCKED_RECT lr = {};
  hr = sys->LockRect(&lr, nullptr, D3DLOCK_READONLY);
  std::printf("  LockRect(SYSTEMMEM, READONLY) hr=0x%08lX pitch=%ld\n", UL(hr), long(lr.Pitch));
  if (FAILED(hr)) { ++c.api_failures; ++c.readback_failures; return; }
  if (!lr.pBits || lr.Pitch < w * 4) {
    std::printf("  invalid mapped data: pBits=%p pitch=%ld need=%d\n", lr.pBits, long(lr.Pitch), w * 4);
    const HRESULT uhr = sys->UnlockRect();
    log_hr("UnlockRect", uhr);
    if (FAILED(uhr)) ++c.api_failures;
    ++c.readback_failures; return;
  }
  const int cx = w / 2, cy = h / 2, qx = 4, qy = 4;
  const BYTE* base = static_cast<const BYTE*>(lr.pBits);
  const D3DCOLOR center = *reinterpret_cast<const D3DCOLOR*>(base + cy * lr.Pitch + cx * 4);
  const D3DCOLOR corner = *reinterpret_cast<const D3DCOLOR*>(base + qy * lr.Pitch + qx * 4);
  const HRESULT uhr = sys->UnlockRect();
  log_hr("UnlockRect", uhr);
  if (FAILED(uhr)) { ++c.api_failures; ++c.readback_failures; }
  const DWORD mask = pixel_mask(format);
  const bool center_ok = (center & mask) == (kTriangleColor & mask);
  const bool corner_ok = (corner & mask) == (kClearColor & mask);
  std::printf("  center(%d,%d) got=0x%08lX masked=0x%08lX want=0x%08lX mask=0x%08lX match=%d\n",
      cx, cy, UL(center), UL(center & mask), UL(kTriangleColor & mask), UL(mask), center_ok);
  std::printf("  corner(%d,%d) got=0x%08lX masked=0x%08lX want=0x%08lX mask=0x%08lX match=%d\n",
      qx, qy, UL(corner), UL(corner & mask), UL(kClearColor & mask), UL(mask), corner_ok);
  if (!center_ok || !corner_ok) ++c.readback_failures;
}

void display_check(HWND window, int w, int h, unsigned frame, Counters& c) {
  ++c.display_checks;
  std::printf("frame=%u stage=post-present composed_display=ON (screen DC GetPixel)\n", frame);
  if (IsIconic(window) || !IsWindowVisible(window)) {
    std::printf("  display FAIL: minimized or not visible; not sampling offscreen pixels\n");
    ++c.display_failures; return;
  }
  POINT center = {w / 2, h / 2}, corner = {4, 4};
  ClientToScreen(window, &center); ClientToScreen(window, &corner);
  HWND ch = WindowFromPoint(center), qh = WindowFromPoint(corner);
  if (ch != window || qh != window) { // also catches offscreen points (null HWND)
    std::printf("  display FAIL: occluded/offscreen center_hwnd=%p corner_hwnd=%p window=%p\n",
        static_cast<void*>(ch), static_cast<void*>(qh), static_cast<void*>(window));
    ++c.display_failures; return;
  }
  HDC screen = GetDC(nullptr);
  if (!screen) { std::printf("  display FAIL: GetDC(nullptr) null\n"); ++c.display_failures; return; }
  const COLORREF cp = GetPixel(screen, center.x, center.y);
  const COLORREF qp = GetPixel(screen, corner.x, corner.y);
  ReleaseDC(nullptr, screen);
  const COLORREF cw = RGB(32, 224, 64), qw = RGB(5, 23, 41);
  const bool cok = cp != CLR_INVALID && cp == cw, qok = qp != CLR_INVALID && qp == qw;
  std::printf("  center screen=(%ld,%ld) COLORREF=0x%08lX want=0x%08lX match=%d\n",
      long(center.x), long(center.y), UL(cp), UL(cw), cok);
  std::printf("  corner screen=(%ld,%ld) COLORREF=0x%08lX want=0x%08lX match=%d\n",
      long(corner.x), long(corner.y), UL(qp), UL(qw), qok);
  if (!cok || !qok) ++c.display_failures;
}

bool apply_render_state(IDirect3DDevice9* dev, Counters& c) {
  HRESULT hr = dev->SetRenderState(D3DRS_MULTISAMPLEANTIALIAS, TRUE);
  log_hr("SetRenderState(D3DRS_MULTISAMPLEANTIALIAS=TRUE)", hr);
  if (!api_step(hr, "SetRenderState(D3DRS_MULTISAMPLEANTIALIAS=TRUE)", c)) return false;
  if (!api_step(dev->SetRenderState(D3DRS_LIGHTING, FALSE),
        "SetRenderState(D3DRS_LIGHTING=FALSE)", c)) return false;
  if (!api_step(dev->SetRenderState(D3DRS_CULLMODE, D3DCULL_NONE),
        "SetRenderState(D3DRS_CULLMODE=NONE)", c)) return false;
  return api_step(dev->SetFVF(D3DFVF_XYZRHW | D3DFVF_DIFFUSE), "SetFVF", c);
}

bool create_readback_surfaces(IDirect3DDevice9* dev, int w, int h, D3DFORMAT format,
    Com<IDirect3DSurface9>& resolve, Com<IDirect3DSurface9>& sysmem, Counters& c) {
  resolve.reset();
  sysmem.reset();
  IDirect3DSurface9* raw = nullptr;
  HRESULT hr = dev->CreateRenderTarget(static_cast<UINT>(w), static_cast<UINT>(h),
      format, D3DMULTISAMPLE_NONE, 0, FALSE, &raw, nullptr);
  log_hr("CreateRenderTarget(single-sample, lockable=FALSE)", hr);
  Com<IDirect3DSurface9> tmp(raw);
  if (FAILED(hr) || !tmp) {
    ++c.api_failures; ++c.readback_setup_failures;
    return false;
  }
  resolve = std::move(tmp);
  raw = nullptr;
  hr = dev->CreateOffscreenPlainSurface(static_cast<UINT>(w), static_cast<UINT>(h),
      format, D3DPOOL_SYSTEMMEM, &raw, nullptr);
  log_hr("CreateOffscreenPlainSurface(SYSTEMMEM)", hr);
  tmp.reset(raw);
  if (FAILED(hr) || !tmp) {
    ++c.api_failures; ++c.readback_setup_failures;
    resolve.reset();
    return false;
  }
  sysmem = std::move(tmp);
  return true;
}

// One phase owns all of its default-pool surfaces locally. They are released when this
// function returns, i.e. before any subsequent Reset, and readback_frame's temporary
// GetBackBuffer COM reference is likewise gone by then.
void run_phase(IDirect3DDevice9* dev, HWND window, bool rb_req, bool screen_check,
    D3DFORMAT format, D3DMULTISAMPLE_TYPE msaa, const PhaseSchedule& sched,
    PhaseResult& pr) {
  Counters& c = pr.counts;
  std::printf("\n-- phase %d (%s) client=%dx%d format=%s msaa=%s --\n",
      pr.index, pr.is_reset ? "reset" : "initial", pr.width, pr.height,
      format_name(format), ms_name(msaa));
  std::printf("phase_state: reapply render state and FVF\n");
  if (!apply_render_state(dev, c)) return;

  Com<IDirect3DSurface9> resolve_rt, sysmem;
  bool rb_ready = false;
  if (rb_req) {
    rb_ready = create_readback_surfaces(dev, pr.width, pr.height, format,
        resolve_rt, sysmem, c);
    if (rb_ready) {
      std::printf("readback_path=MSAA backbuffer -> StretchRect(full-size, filter=NONE) "
                  "-> single-sample RT -> GetRenderTargetData -> SYSTEMMEM\n");
      std::printf("pixel_mask=0x%08lX (%s alpha %s)\n", UL(pixel_mask(format)),
          format_name(format), format == D3DFMT_A8R8G8B8 ? "compared" : "ignored");
    } else {
      std::printf("readback setup incomplete; phase cannot satisfy readback oracle\n");
    }
  } else {
    std::printf("readback_path=disabled (--present-only); no GetBackBuffer/StretchRect/GetRenderTargetData\n");
  }

  const float w = static_cast<float>(pr.width), h = static_cast<float>(pr.height);
  const Vertex vertices[] = {
      {0.5f * w, 0.1f * h, 0.0f, 1.0f, kTriangleColor},
      {0.1f * w, 0.9f * h, 0.0f, 1.0f, kTriangleColor},
      {0.9f * w, 0.9f * h, 0.0f, 1.0f, kTriangleColor}};

  for (unsigned frame = 0; frame < sched.frames; ++frame) {
    if (!pump_messages()) { pr.quit_seen = true; return; }
    if (!api_step(dev->Clear(0, nullptr, D3DCLEAR_TARGET, kClearColor, 1.0f, 0), "Clear", c)) return;
    if (!api_step(dev->BeginScene(), "BeginScene", c)) return;
    if (!api_step(dev->DrawPrimitiveUP(D3DPT_TRIANGLELIST, 1, vertices, sizeof(Vertex)),
          "DrawPrimitiveUP", c)) {
      dev->EndScene();
      return;
    }
    if (!api_step(dev->EndScene(), "EndScene", c)) return;
    if (rb_ready) {
      for (unsigned i = 0; i < kReadbackFrameCount; ++i) {
        if (frame == sched.readback_frames[i])
          readback_frame(dev, resolve_rt.get(), sysmem.get(), format, msaa,
              pr.width, pr.height, frame, c);
      }
    }
    const HRESULT hr = dev->Present(nullptr, nullptr, nullptr, nullptr);
    if (frame == 0) std::printf("first Present hr=0x%08lX\n", UL(hr));
    if (FAILED(hr)) { std::printf("FAIL Present hr=0x%08lX\n", UL(hr)); ++c.api_failures; return; }
    if (screen_check) {
      for (unsigned i = 0; i < kScreenFrameCount; ++i) {
        if (frame != sched.display_frames[i]) continue;
        if (!pump_for(80)) { pr.quit_seen = true; return; } // let DWM compose, then sample
        display_check(window, pr.width, pr.height, frame, c);
      }
    }
    Sleep(20);
  }
  if (!pump_messages()) { pr.quit_seen = true; return; } // catch a late WM_QUIT
  pr.complete = true;
}

bool phase_pass(const PhaseResult& pr, bool rb_req, bool screen) {
  if (!pr.reset_ok || !pr.complete || pr.counts.api_failures > 0) return false;
  if (rb_req && (pr.counts.readback_setup_failures > 0 ||
      pr.counts.readback_checks < int(kReadbackFrameCount) ||
      pr.counts.readback_failures > 0)) return false;
  if (screen && (pr.counts.display_checks < int(kScreenFrameCount) ||
      pr.counts.display_failures > 0)) return false;
  return true;
}

void print_phase(const PhaseResult& pr, bool rb_req, bool screen) {
  const char* rb = !rb_req ? "DISABLED" :
      pr.counts.readback_setup_failures > 0 ? "SETUP_FAILED" :
      pr.counts.readback_checks < int(kReadbackFrameCount) ? "INCOMPLETE" :
      pr.counts.readback_failures > 0 ? "FAIL" : "PASS";
  const char* disp = !screen ? "DISABLED" :
      pr.counts.display_checks < int(kScreenFrameCount) ? "INCOMPLETE" :
      pr.counts.display_failures > 0 ? "FAIL" : "PASS";
  const char* result = phase_pass(pr, rb_req, screen) ? "PASS" : "FAIL";
  if (pr.is_reset) {
    std::printf("PHASE %d reset %dx%d reset_hr=0x%08lX complete=%d gpu_readback=%s "
                "checks=%d failures=%d composed_display=%s checks=%d failures=%d result=%s\n",
        pr.index, pr.width, pr.height, UL(pr.reset_hr), pr.complete, rb,
        pr.counts.readback_checks, pr.counts.readback_failures, disp,
        pr.counts.display_checks, pr.counts.display_failures, result);
  } else {
    std::printf("PHASE %d initial %dx%d gpu_readback=%s checks=%d failures=%d "
                "composed_display=%s checks=%d failures=%d result=%s\n",
        pr.index, pr.width, pr.height, rb, pr.counts.readback_checks,
        pr.counts.readback_failures, disp, pr.counts.display_checks,
        pr.counts.display_failures, result);
  }
}

int print_summary(const Counters& c, bool rb_req, bool screen,
    int resets_requested, int resets_completed, bool quit_seen,
    unsigned phases_run, int phase_failures) {
  const int expected_rb = rb_req ? int(kReadbackFrameCount * phases_run) : 0;
  const int expected_disp = screen ? int(kScreenFrameCount * phases_run) : 0;
  const bool rb_incomplete = rb_req && c.readback_checks < expected_rb;
  const bool disp_incomplete = screen && c.display_checks < expected_disp;
  const bool reset_incomplete = resets_requested > 0 && resets_completed < resets_requested;
  const char* rb_status = !rb_req ? "DISABLED" :
      c.readback_setup_failures > 0 ? "SETUP_FAILED" :
      rb_incomplete ? "INCOMPLETE" : c.readback_failures > 0 ? "FAIL" : "PASS";
  const char* disp_status = !screen ? "DISABLED" :
      disp_incomplete ? "INCOMPLETE" : c.display_failures > 0 ? "FAIL" : "PASS";
  const bool api_failure = c.api_failures > 0;
  const bool oracle_failure = c.readback_failures > 0 || c.display_failures > 0;
  const bool aborted = quit_seen || phase_failures > 0 || reset_incomplete;
  const int code = api_failure ? 1 :
      (oracle_failure || rb_incomplete || disp_incomplete || aborted) ? 2 : 0;
  std::printf("\nSUMMARY gpu_readback=%s checks=%d/%d failures=%d\n", rb_status,
      c.readback_checks, expected_rb, c.readback_failures);
  std::printf("SUMMARY composed_display=%s checks=%d/%d failures=%d\n", disp_status,
      c.display_checks, expected_disp, c.display_failures);
  if (resets_requested > 0 || resets_completed > 0) {
    std::printf("SUMMARY resets=%d/%d completed phases=%u failed_phases=%d "
                "reset_incomplete=%d quit=%d\n",
        resets_completed, resets_requested, phases_run, phase_failures,
        reset_incomplete ? 1 : 0, quit_seen ? 1 : 0);
  }
  std::printf("SUMMARY api_failures=%d result=%s exit=%d\n", c.api_failures,
      code == 0 ? "PASS" : code == 1 ? "API_FAILURE" : "ORACLE_FAILURE", code);
  return code;
}

D3DPRESENT_PARAMETERS make_present_params(HWND window, int w, int h, D3DFORMAT format,
    D3DMULTISAMPLE_TYPE msaa) {
  D3DPRESENT_PARAMETERS pp = {};
  pp.BackBufferWidth = static_cast<UINT>(w);
  pp.BackBufferHeight = static_cast<UINT>(h);
  pp.BackBufferFormat = format;
  pp.BackBufferCount = 1;
  pp.MultiSampleType = msaa;
  pp.MultiSampleQuality = 0;
  pp.SwapEffect = D3DSWAPEFFECT_DISCARD; // required for nonzero MSAA; no COPY/lockable
  pp.hDeviceWindow = window;
  pp.Windowed = TRUE;
  pp.PresentationInterval = D3DPRESENT_INTERVAL_IMMEDIATE;
  pp.Flags = 0;
  return pp;
}

bool resize_window_client(HWND window, int w, int h) {
  RECT rect = {0, 0, w, h};
  const DWORD style = static_cast<DWORD>(GetWindowLongW(window, GWL_STYLE));
  const DWORD exstyle = static_cast<DWORD>(GetWindowLongW(window, GWL_EXSTYLE));
  if (!AdjustWindowRectEx(&rect, style, FALSE, exstyle)) return false;
  if (!SetWindowPos(window, HWND_TOPMOST, 0, 0, rect.right - rect.left,
        rect.bottom - rect.top, SWP_NOMOVE | SWP_SHOWWINDOW)) return false;
  UpdateWindow(window);
  return true;
}
} // namespace

int main(int argc, char** argv) {
  int samples = 4, reset_cycles = 0;
  const char* log_path = nullptr;
  bool present_only = false, screen_check = true, use_argb = false;
  for (int i = 1; i < argc; ++i) {
    if (std::strcmp(argv[i], "--samples") == 0 && i + 1 < argc) {
      if (!parse_samples_arg(argv[++i], samples)) {
        std::fprintf(stderr, "invalid --samples '%s' (expected 1, 2, 4, or 8)\n", argv[i]);
        return 1;
      }
    } else if (std::strcmp(argv[i], "--reset-cycles") == 0 && i + 1 < argc) {
      if (!parse_reset_arg(argv[++i], reset_cycles)) {
        std::fprintf(stderr, "invalid --reset-cycles '%s' (expected 1..%d)\n",
            argv[i], kMaxResetCycles);
        return 1;
      }
    } else if (std::strcmp(argv[i], "--log") == 0 && i + 1 < argc) log_path = argv[++i];
    else if (std::strcmp(argv[i], "--present-only") == 0) present_only = true;
    else if (std::strcmp(argv[i], "--no-screen-check") == 0) screen_check = false;
    else if (std::strcmp(argv[i], "--argb") == 0) use_argb = true;
    else if (std::strcmp(argv[i], "--help") == 0) {
      std::printf("usage: %s [--samples 1|2|4|8] [--argb] [--reset-cycles 1..8]\n"
                  "          [--log PATH] [--present-only] [--no-screen-check]\n", argv[0]);
      return 0;
    } else {
      std::fprintf(stderr, "unknown argument: %s\n", argv[i]);
      return 1;
    }
  }
  if (present_only && !screen_check) {
    std::fprintf(stderr, "--present-only cannot be combined with --no-screen-check (no oracle)\n");
    return 1;
  }
  if (samples != 1 && samples != 2 && samples != 4 && samples != 8) {
    std::fprintf(stderr, "invalid --samples %d (expected 1, 2, 4, or 8)\n", samples);
    return 1;
  }
  if (log_path && !std::freopen(log_path, "w", stdout)) {
    std::fprintf(stderr, "cannot open log path: %s\n", log_path);
    return 1;
  }
  std::setvbuf(stdout, nullptr, _IONBF, 0);
  SetProcessDPIAware();
  if (screen_check) { // HELIOS_NVK_PRESENT=2 forces DWM composition (not direct scanout)
    SetEnvironmentVariableA("HELIOS_NVK_PRESENT", "2");
    std::printf("HELIOS_NVK_PRESENT=2 (forced DWM composition for screen checks)\n");
  } else {
    char policy[32] = {};
    const DWORD n = GetEnvironmentVariableA("HELIOS_NVK_PRESENT", policy, sizeof(policy));
    std::printf("HELIOS_NVK_PRESENT=%s (caller policy preserved; no screen checks)\n",
        (n > 0 && n < sizeof(policy)) ? policy : "<unset>");
  }

  const D3DFORMAT format = use_argb ? D3DFMT_A8R8G8B8 : D3DFMT_X8R8G8B8;
  const D3DMULTISAMPLE_TYPE msaa = ms_type_for(samples);
  const bool rb_req = !present_only;
  if (reset_cycles > 0) {
    std::printf("d3d9_msaa_present samples=%d msaa=%s format=%s mode=%s screen_check=%s "
                "reset_cycles=%d\n", samples, ms_name(msaa), format_name(format),
        present_only ? "present-only" : "readback", screen_check ? "ON" : "OFF",
        reset_cycles);
  } else {
    std::printf("d3d9_msaa_present samples=%d msaa=%s format=%s mode=%s screen_check=%s\n",
        samples, ms_name(msaa), format_name(format),
        present_only ? "present-only" : "readback", screen_check ? "ON" : "OFF");
  }
  PhaseSchedule sched = {};
  if (reset_cycles > 0) {
    sched.frames = kResetPhaseFrames;
    sched.readback_frames[0] = 0; sched.readback_frames[1] = 15; sched.readback_frames[2] = 30;
    sched.display_frames[0] = 10; sched.display_frames[1] = 25; sched.display_frames[2] = 35;
    std::printf("per_phase_frames=%u readback_frames=0,15,30 (frame 0 before first Present); "
                "screen_frames=10,25,35 (post-present); reset_dims=%dx%d/%dx%d\n",
        sched.frames, kInitWidth, kInitHeight, kAltWidth, kAltHeight);
  } else {
    sched.frames = kFrames;
    sched.readback_frames[0] = 0; sched.readback_frames[1] = 30; sched.readback_frames[2] = 50;
    sched.display_frames[0] = 20; sched.display_frames[1] = 40; sched.display_frames[2] = 60;
    std::printf("readback_frames=0,30,50 (pre-present; 0 before first Present); "
                "screen_frames=20,40,60 (post-present)\n");
  }

  Com<IDirect3D9> d3d(Direct3DCreate9(D3D_SDK_VERSION));
  if (!d3d) return api_fail_msg("Direct3DCreate9 returned null");
  D3DADAPTER_IDENTIFIER9 ident = {}; HRESULT hr = d3d->GetAdapterIdentifier(D3DADAPTER_DEFAULT, 0, &ident);
  if (FAILED(hr)) return api_fail("GetAdapterIdentifier", hr);
  std::printf("adapter desc=\"%s\" driver=\"%s\" vendor=0x%08lX device=0x%08lX\n",
      ident.Description, ident.Driver, UL(ident.VendorId), UL(ident.DeviceId));
  print_modules("startup");

  DWORD quality_levels = 0;
  hr = d3d->CheckDeviceMultiSampleType(D3DADAPTER_DEFAULT, D3DDEVTYPE_HAL, format, TRUE, msaa, &quality_levels);
  std::printf("CheckDeviceMultiSampleType(HAL, format=%s, windowed=TRUE, msaa=%s) "
              "hr=0x%08lX qualityLevels=%lu\n", format_name(format), ms_name(msaa),
      UL(hr), UL(quality_levels));
  if (hr == D3DERR_NOTAVAILABLE ||
      (SUCCEEDED(hr) && quality_levels == 0 && msaa != D3DMULTISAMPLE_NONE)) {
    std::printf("SKIP: MSAA=%s unsupported for exact windowed format %s "
                "(hr=0x%08lX qualityLevels=%lu)\n",
        ms_name(msaa), format_name(format), UL(hr), UL(quality_levels));
    std::printf("SUMMARY gpu_readback=NOT_RUN composed_display=NOT_RUN "
                "api_failures=0 result=SKIP exit=77\n");
    return 77;
  }
  if (FAILED(hr)) return api_fail("CheckDeviceMultiSampleType", hr);

  WNDCLASSW wc = {};
  wc.lpfnWndProc = wnd_proc; wc.hInstance = GetModuleHandleW(nullptr);
  wc.hCursor = LoadCursorA(nullptr, IDC_ARROW); wc.lpszClassName = L"D3D9MsaaPresentProbe";
  wc.hbrBackground = static_cast<HBRUSH>(GetStockObject(BLACK_BRUSH));
  if (!RegisterClassW(&wc)) { std::printf("FAIL RegisterClassW err=%lu\n", UL(GetLastError())); return 1; }
  WindowGuard window_guard; window_guard.cls = wc.lpszClassName;
  const DWORD style = WS_OVERLAPPEDWINDOW;
  RECT rect = {0, 0, kInitWidth, kInitHeight}; AdjustWindowRectEx(&rect, style, FALSE, WS_EX_TOPMOST);
  window_guard.hwnd = CreateWindowExW(WS_EX_TOPMOST, wc.lpszClassName, L"D3D9 MSAA present probe",
      style | WS_VISIBLE, 100, 100, rect.right - rect.left, rect.bottom - rect.top,
      nullptr, nullptr, wc.hInstance, nullptr);
  if (!window_guard.hwnd) { std::printf("FAIL CreateWindowExW err=%lu\n", UL(GetLastError())); return 1; }
  HWND window = window_guard.hwnd;
  ShowWindow(window, SW_SHOW); UpdateWindow(window); SetForegroundWindow(window);
  SetWindowPos(window, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW);
  RECT client = {}; GetClientRect(window, &client);
  int width = client.right - client.left, height = client.bottom - client.top;
  const int base_width = width, base_height = height;
  std::printf("window=%p client=%dx%d topmost=TRUE\n", static_cast<void*>(window), width, height);
  if (width <= 0 || height <= 0) return api_fail_msg("invalid client size");

  D3DPRESENT_PARAMETERS pp = make_present_params(window, width, height, format, msaa);
  std::printf("present_params: %ux%u format=%s windowed=TRUE msaa=%s quality=0 "
              "swap_effect=DISCARD backbuffer_count=1 flags=0 lockable_msaa=FALSE\n",
      pp.BackBufferWidth, pp.BackBufferHeight, format_name(pp.BackBufferFormat),
      ms_name(pp.MultiSampleType));

  IDirect3DDevice9* device_raw = nullptr;
  hr = d3d->CreateDevice(D3DADAPTER_DEFAULT, D3DDEVTYPE_HAL, window,
      D3DCREATE_HARDWARE_VERTEXPROCESSING, &pp, &device_raw);
  std::printf("CreateDevice(HAL, hardware vertex processing) hr=0x%08lX\n", UL(hr));
  Com<IDirect3DDevice9> device(device_raw);
  if (FAILED(hr) || !device) return api_fail("CreateDevice", hr);
  print_modules("post-device");

  PhaseResult phases[kMaxPhases];
  int phase_count = 0, phase_failures = 0, resets_completed = 0;
  Counters totals;
  bool quit_seen = false, stop = false;

  auto finish_phase = [&](PhaseResult& pr) {
    add_counts(totals, pr.counts);
    quit_seen = quit_seen || pr.quit_seen;
    if (!phase_pass(pr, rb_req, screen_check)) ++phase_failures;
    if (pr.quit_seen || !pr.complete) stop = true;
  };

  PhaseResult& initial = phases[phase_count++];
  initial.index = 0; initial.is_reset = false;
  initial.width = width; initial.height = height;
  run_phase(device.get(), window, rb_req, screen_check, format, msaa, sched, initial);
  finish_phase(initial);
  if (initial.quit_seen) std::printf("WM_QUIT observed; stopping and cleaning up\n");

  for (int cycle = 1; cycle <= reset_cycles && !stop; ++cycle) {
    const bool use_alt = (cycle % 2) == 1;
    const int want_w = use_alt ? kAltWidth : base_width;
    const int want_h = use_alt ? kAltHeight : base_height;
    std::printf("\n-- reset_cycle %d/%d: resize client %dx%d -> %dx%d --\n",
        cycle, reset_cycles, width, height, want_w, want_h);
    if (!resize_window_client(window, want_w, want_h)) {
      std::printf("FAIL resize_window_client err=%lu\n", UL(GetLastError()));
      ++totals.api_failures;
      break;
    }
    RECT cr = {}; GetClientRect(window, &cr);
    width = cr.right - cr.left; height = cr.bottom - cr.top;
    if (width != want_w || height != want_h) {
      std::printf("FAIL client size after resize %dx%d, expected %dx%d\n",
          width, height, want_w, want_h);
      ++totals.api_failures;
      break;
    }
    std::printf("client_after_resize=%dx%d\n", width, height);
    std::printf("reset_precheck: prior phase default-pool surfaces and backbuffer refs already released\n");

    const HRESULT coop_before = device->TestCooperativeLevel();
    log_hr("TestCooperativeLevel(before Reset)", coop_before);
    D3DPRESENT_PARAMETERS rpp = make_present_params(window, width, height, format, msaa);
    const HRESULT reset_hr = device->Reset(&rpp);
    std::printf("Reset(%ux%u format=%s msaa=%s quality=0 swap_effect=DISCARD) hr=0x%08lX\n",
        rpp.BackBufferWidth, rpp.BackBufferHeight, format_name(rpp.BackBufferFormat),
        ms_name(rpp.MultiSampleType), UL(reset_hr));

    PhaseResult& reset_phase = phases[phase_count++];
    reset_phase.index = cycle; reset_phase.is_reset = true;
    reset_phase.width = width; reset_phase.height = height;
    reset_phase.reset_hr = reset_hr; reset_phase.reset_ok = SUCCEEDED(reset_hr);
    const HRESULT coop_after = device->TestCooperativeLevel();
    log_hr("TestCooperativeLevel(after Reset)", coop_after);
    if (FAILED(reset_hr) || FAILED(coop_after)) {
      ++reset_phase.counts.api_failures;
      finish_phase(reset_phase);
      std::printf("Reset or subsequent TestCooperativeLevel failed; stopping\n");
      break;
    }
    ++resets_completed;
    run_phase(device.get(), window, rb_req, screen_check, format, msaa, sched, reset_phase);
    finish_phase(reset_phase);
    if (reset_phase.quit_seen) std::printf("WM_QUIT observed; stopping and cleaning up\n");
  }

  if (reset_cycles > 0) {
    std::printf("\n-- per-phase results --\n");
    for (int i = 0; i < phase_count; ++i) print_phase(phases[i], rb_req, screen_check);
  }
  print_modules("post-run");
  return print_summary(totals, rb_req, screen_check, reset_cycles, resets_completed,
      quit_seen, static_cast<unsigned>(phase_count), phase_failures);
}
