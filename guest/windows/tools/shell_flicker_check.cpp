// shell_flicker_check: an objective check for taskbar / Start-menu flicker.
//
//   shell_flicker_check.exe [ROUNDS=6 [OUTDIR=.]]
//
// Run it in the interactive session (tools/run-in-session.ps1); it drives the shell itself, so
// do not use the desktop meanwhile. It records every frame DWM composes (DXGI desktop
// duplication of output 0) while it drives the shell with SendInput, ROUNDS times each:
//   start   the Windows key (Start opens), Esc (it closes)
//   search  Win+S (the search pane), Esc
//   quick   Win+A (quick settings), Esc
//   notify  Win+N (notifications), Esc
//   explorer Win+E (a new File Explorer window), then that window is closed (WM_CLOSE to the
//           CabinetWClass window that appeared, never any other window)
//   taskbar a left click on the taskbar clock (bottom right: the calendar flyout), Esc
//
// Every composed frame is sampled on a grid (every 16th pixel of every 16th row). A flicker is a
// TRANSIENT frame: frame B differs from both its neighbours A and C in at least
// MIN_CELLS grid cells while A and C agree with each other (a frame that comes and goes again);
// an animation changes monotonically and does not count. A transient of fewer than kLargeCells
// cells is SMALL (a caret, a hover): listed and counted apart, never a failure. A transient in
// which at least half the
// changed cells went black is counted as BLACK as well. The first few transients are written to
// OUTDIR as BMP files (the transient frame and the one before it).
//
// The check sees what DWM composes. A frame that DWM composed right but the display path showed
// wrong (a flip retired early, a stale buffer scanned out) is invisible to it: that is a KMD /
// host problem and needs the user's eyes or a host-side capture.
//
// Prints per action the frames and transients, and a summary line; exit code 0 when no
// transient was seen, 2 otherwise, 1 when the duplication could not start.
//
// Build (one line): x86_64-w64-mingw32-g++ -std=c++17 -O2 -static shell_flicker_check.cpp
//   -o shell_flicker_check.exe -ld3d11 -ldxgi -luser32 -lgdi32
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11.h>
#include <dxgi1_2.h>
#include <atomic>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <thread>
#include <vector>

static const UINT kStep = 16;
static const UINT kMinCells = 24;
// A transient smaller than this many cells is SMALL (a text caret blinking, a hover highlight):
// listed, but not a failure.
static const UINT kLargeCells = 48;
static const UINT kCellTol = 24;  // per-channel difference that counts as a change

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

// ---- input ----------------------------------------------------------------------------------

static void key(WORD vk, bool up) {
  INPUT in = {};
  in.type = INPUT_KEYBOARD;
  in.ki.wVk = vk;
  in.ki.dwFlags = up ? KEYEVENTF_KEYUP : 0;
  if (vk == VK_LWIN) in.ki.dwFlags |= KEYEVENTF_EXTENDEDKEY;
  SendInput(1, &in, sizeof(in));
}
static void tap(WORD vk) {
  key(vk, false);
  Sleep(30);
  key(vk, true);
}
static void chord(WORD mod, WORD vk) {
  key(mod, false);
  Sleep(30);
  tap(vk);
  Sleep(30);
  key(mod, true);
}
static void click(int x, int y) {
  const int sw = GetSystemMetrics(SM_CXSCREEN), sh = GetSystemMetrics(SM_CYSCREEN);
  INPUT in[3] = {};
  in[0].type = INPUT_MOUSE;
  in[0].mi.dx = LONG(x * 65535 / (sw - 1));
  in[0].mi.dy = LONG(y * 65535 / (sh - 1));
  in[0].mi.dwFlags = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE;
  in[1].type = INPUT_MOUSE;
  in[1].mi.dwFlags = MOUSEEVENTF_LEFTDOWN;
  in[2].type = INPUT_MOUSE;
  in[2].mi.dwFlags = MOUSEEVENTF_LEFTUP;
  SendInput(3, in, sizeof(INPUT));
}

struct Windows {
  std::vector<HWND> list;
};
static BOOL CALLBACK collect(HWND h, LPARAM p) {
  char cls[64];
  if (GetClassNameA(h, cls, sizeof(cls)) && !std::strcmp(cls, "CabinetWClass") && IsWindowVisible(h))
    reinterpret_cast<Windows *>(p)->list.push_back(h);
  return TRUE;
}
static std::vector<HWND> explorer_windows() {
  Windows w;
  EnumWindows(collect, reinterpret_cast<LPARAM>(&w));
  return w.list;
}

// ---- capture --------------------------------------------------------------------------------

struct Stats {
  std::atomic<unsigned> frames{0}, transients{0}, black{0}, small{0};
};
static const char *g_action = "idle";
static std::atomic<bool> g_stop{false};
static Stats g_total;
static unsigned g_saved = 0;

static void save_bmp(const std::string &path, const std::vector<UINT32> &px, UINT w, UINT h) {
  BITMAPFILEHEADER fh = {};
  BITMAPINFOHEADER ih = {};
  ih.biSize = sizeof(ih);
  ih.biWidth = LONG(w);
  ih.biHeight = -LONG(h);
  ih.biPlanes = 1;
  ih.biBitCount = 32;
  fh.bfType = 0x4d42;
  fh.bfOffBits = sizeof(fh) + sizeof(ih);
  fh.bfSize = fh.bfOffBits + DWORD(px.size() * 4);
  FILE *f = std::fopen(path.c_str(), "wb");
  if (!f) return;
  std::fwrite(&fh, sizeof(fh), 1, f);
  std::fwrite(&ih, sizeof(ih), 1, f);
  std::fwrite(px.data(), 4, px.size(), f);
  std::fclose(f);
}

static bool differs(UINT32 a, UINT32 b) {
  for (int s = 0; s < 24; s += 8) {
    const int x = int((a >> s) & 0xff), y = int((b >> s) & 0xff);
    if (std::abs(x - y) > int(kCellTol)) return true;
  }
  return false;
}

static int capture(const std::string &outdir, Stats *per_action, const char *const *names, int nactions) {
  ID3D11Device *dev = nullptr;
  ID3D11DeviceContext *ctx = nullptr;
  if (FAILED(D3D11CreateDevice(nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr, 0, nullptr, 0, D3D11_SDK_VERSION, &dev,
                               nullptr, &ctx))) {
    std::printf("[FAIL] D3D11CreateDevice\n");
    return 1;
  }
  IDXGIDevice *dd = nullptr;
  dev->QueryInterface(__uuidof(IDXGIDevice), reinterpret_cast<void **>(&dd));
  IDXGIAdapter *ad = nullptr;
  dd->GetAdapter(&ad);
  IDXGIOutput *out = nullptr;
  ad->EnumOutputs(0, &out);
  IDXGIOutput1 *out1 = nullptr;
  if (!out || FAILED(out->QueryInterface(__uuidof(IDXGIOutput1), reinterpret_cast<void **>(&out1)))) {
    std::printf("[FAIL] no output 0\n");
    return 1;
  }
  IDXGIOutputDuplication *dup = nullptr;
  HRESULT hr = out1->DuplicateOutput(dev, &dup);
  if (FAILED(hr)) {
    std::printf("[FAIL] DuplicateOutput: 0x%08lx\n", static_cast<unsigned long>(hr));
    return 1;
  }
  DXGI_OUTDUPL_DESC od;
  dup->GetDesc(&od);
  const UINT W = od.ModeDesc.Width, H = od.ModeDesc.Height;
  const UINT gw = W / kStep, gh = H / kStep;
  std::printf("capturing %ux%u, grid %ux%u\n", W, H, gw, gh);
  D3D11_TEXTURE2D_DESC sd = {};
  sd.Width = W;
  sd.Height = H;
  sd.MipLevels = 1;
  sd.ArraySize = 1;
  sd.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
  sd.SampleDesc.Count = 1;
  sd.Usage = D3D11_USAGE_STAGING;
  sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
  ID3D11Texture2D *staging = nullptr;
  dev->CreateTexture2D(&sd, nullptr, &staging);
  std::vector<UINT32> g[3];
  for (auto &v : g) v.assign(size_t(gw) * gh, 0);
  std::vector<UINT32> full[2];
  int have = 0, cur = 0;
  unsigned fno = 0;  // frames sampled: the full-frame slot is fno & 1
  const char *action_of[3] = {"", "", ""};
  while (!g_stop.load()) {
    DXGI_OUTDUPL_FRAME_INFO fi;
    IDXGIResource *res = nullptr;
    hr = dup->AcquireNextFrame(50, &fi, &res);
    if (hr == DXGI_ERROR_WAIT_TIMEOUT) continue;
    if (hr == DXGI_ERROR_ACCESS_LOST) {
      // A mode change or a secure desktop: start again.
      release(dup);
      Sleep(100);
      if (FAILED(out1->DuplicateOutput(dev, &dup))) return 1;
      have = 0;
      continue;
    }
    if (FAILED(hr)) break;
    if (fi.LastPresentTime.QuadPart == 0) {
      // A cursor-only update: no new image.
      release(res);
      dup->ReleaseFrame();
      continue;
    }
    ID3D11Texture2D *tex = nullptr;
    res->QueryInterface(__uuidof(ID3D11Texture2D), reinterpret_cast<void **>(&tex));
    ctx->CopyResource(staging, tex);
    release(tex);
    release(res);
    dup->ReleaseFrame();
    D3D11_MAPPED_SUBRESOURCE m;
    if (FAILED(ctx->Map(staging, 0, D3D11_MAP_READ, 0, &m))) continue;
    std::vector<UINT32> &s = g[cur];
    for (UINT y = 0; y < gh; y++) {
      const UINT32 *row = reinterpret_cast<const UINT32 *>(static_cast<const char *>(m.pData) + size_t(y * kStep) * m.RowPitch);
      for (UINT x = 0; x < gw; x++) s[size_t(y) * gw + x] = row[x * kStep] & 0xffffffu;
    }
    // Keep the last two full frames for a saved transient.
    if (g_saved < 4) {
      std::vector<UINT32> &f = full[fno & 1];
      f.resize(size_t(W) * H);
      for (UINT y = 0; y < H; y++)
        std::memcpy(f.data() + size_t(y) * W, static_cast<const char *>(m.pData) + size_t(y) * m.RowPitch, W * 4);
    }
    ctx->Unmap(staging, 0);
    action_of[cur] = g_action;
    for (int i = 0; i < nactions; i++)
      if (!std::strcmp(names[i], g_action)) per_action[i].frames++;
    g_total.frames++;
    have = have < 3 ? have + 1 : 3;
    if (have == 3) {
      const std::vector<UINT32> &a = g[(cur + 1) % 3], &b = g[(cur + 2) % 3], &c = g[cur];
      UINT ab = 0, cb = 0, ac = 0, blackened = 0;
      for (size_t i = 0; i < a.size(); i++) {
        const bool d1 = differs(a[i], b[i]), d2 = differs(c[i], b[i]), d3 = differs(a[i], c[i]);
        ab += d1;
        cb += d2;
        ac += d3;
        if (d1 && d2 && !d3 && b[i] == 0 && a[i] != 0) blackened++;
      }
      if (ab >= kMinCells && cb >= kMinCells && ac * 4 < ab) {
        const char *act = action_of[(cur + 2) % 3];
        const bool is_black = blackened * 2 >= ab;
        const bool small = ab < kLargeCells && cb < kLargeCells;
        if (small) {
          g_total.small++;
        } else {
          g_total.transients++;
          if (is_black) g_total.black++;
        }
        for (int i = 0; i < nactions; i++)
          if (!std::strcmp(names[i], act)) {
            if (small) {
              per_action[i].small++;
            } else {
              per_action[i].transients++;
              if (is_black) per_action[i].black++;
            }
          }
        std::printf("[TRANSIENT%s] t=%.0f ms action=%s cells %u (vs next %u, prev/next agree but %u), black %u\n",
                    small ? " small" : "", now_ms(), act, ab, cb, ac, blackened);
        if (!small && g_saved < 4 && full[0].size() == full[1].size() && !full[0].empty()) {
          // The transient frame B is the previous full frame; save it and the current one.
          char p[MAX_PATH];
          std::snprintf(p, sizeof(p), "%s\\flicker-%u-transient.bmp", outdir.c_str(), g_saved);
          save_bmp(p, full[(fno + 1) & 1], W, H);
          std::snprintf(p, sizeof(p), "%s\\flicker-%u-after.bmp", outdir.c_str(), g_saved);
          save_bmp(p, full[fno & 1], W, H);
          g_saved++;
        }
      }
    }
    cur = (cur + 1) % 3;
    fno++;
  }
  release(dup);
  release(staging);
  release(out1);
  release(out);
  release(ad);
  release(dd);
  release(ctx);
  release(dev);
  return 0;
}

int main(int argc, char **argv) {
  const int rounds = argc > 1 ? std::atoi(argv[1]) : 6;
  const std::string outdir = argc > 2 ? argv[2] : ".";
  static const char *const names[] = {"start", "search", "quick", "notify", "explorer", "taskbar"};
  const int n = int(sizeof(names) / sizeof(names[0]));
  Stats per[sizeof(names) / sizeof(names[0])];
  int rc = 0;
  std::thread cap([&] { rc = capture(outdir, per, names, n); });
  Sleep(1500);
  const int sw = GetSystemMetrics(SM_CXSCREEN), sh = GetSystemMetrics(SM_CYSCREEN);
  for (int r = 0; r < rounds && rc == 0; r++) {
    g_action = "start";
    tap(VK_LWIN);
    Sleep(1500);
    tap(VK_ESCAPE);
    Sleep(1000);
    g_action = "search";
    chord(VK_LWIN, 'S');
    Sleep(1500);
    tap(VK_ESCAPE);
    Sleep(1000);
    g_action = "quick";
    chord(VK_LWIN, 'A');
    Sleep(1500);
    tap(VK_ESCAPE);
    Sleep(1000);
    g_action = "notify";
    chord(VK_LWIN, 'N');
    Sleep(1500);
    tap(VK_ESCAPE);
    Sleep(1000);
    g_action = "explorer";
    const std::vector<HWND> before = explorer_windows();
    chord(VK_LWIN, 'E');
    Sleep(2500);
    for (HWND h : explorer_windows()) {
      bool old = false;
      for (HWND b : before) old |= b == h;
      if (!old) PostMessageW(h, WM_CLOSE, 0, 0);
    }
    Sleep(1500);
    g_action = "taskbar";
    click(sw - 60, sh - 20);
    Sleep(1500);
    tap(VK_ESCAPE);
    Sleep(1000);
    g_action = "idle";
  }
  Sleep(500);
  g_stop = true;
  cap.join();
  if (rc) return rc;
  for (int i = 0; i < n; i++)
    std::printf("%-9s frames %5u transients %3u black %3u small %3u\n", names[i], per[i].frames.load(),
                per[i].transients.load(), per[i].black.load(), per[i].small.load());
  const bool clean = g_total.transients.load() == 0;
  std::printf("%s: %d rounds, frames %u, transients %u (black %u), small %u\n", clean ? "[PASS]" : "[FAIL]", rounds,
              g_total.frames.load(), g_total.transients.load(), g_total.black.load(), g_total.small.load());
  return clean ? 0 : 2;
}
