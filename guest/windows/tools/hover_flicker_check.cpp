// hover_flicker_check: an objective check for black areas while the mouse moves over a window.
//
//   hover_flicker_check.exe TITLE [SECONDS=20 [OUTDIR=. [SKIPTOP=40]]]
//
// Finds the top-level window whose title contains TITLE (for example a Chromium/Edge window showing
// a page with hover effects), moves the cursor over its client area in a zig-zag (SetCursorPos, about
// 250 moves per second; Chromium sees WM_MOUSEMOVE and repaints the hovered tiles), and meanwhile
// captures the client area from the composed desktop (BitBlt of the screen DC: what DWM shows) as fast
// as it can. A capture in which at least kMinBlack more sampled pixels than in the least
// black capture of the run (every 4th pixel of every 4th row, below the top SKIPTOP rows: a Chromium app window draws its own
// caption, with black text, inside the client area) are black (r, g and b all below 8) is a BLACK capture, the
// "black tiles" the user sees. The first few are written to OUTDIR as BMP files, and one ordinary
// capture from the middle of the run as hover-sample.bmp.
//
// Prints per second the captures and black captures, and a summary line; exit code 0 when no black
// capture was seen, 2 otherwise, 1 when the window was not found. The cursor is put back afterwards.
//
// Build (one line): x86_64-w64-mingw32-g++ -std=c++17 -O2 -static hover_flicker_check.cpp
//   -o hover_flicker_check.exe -luser32 -lgdi32
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <atomic>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <thread>

// A capture is BLACK when it has this many black samples more than the fewest any capture had
// (the window's own black parts, a logo or caption text, are in every capture).
static const unsigned kMinBlack = 32;

static double now_ms() {
  static LARGE_INTEGER f = [] { LARGE_INTEGER x; QueryPerformanceFrequency(&x); return x; }();
  LARGE_INTEGER t;
  QueryPerformanceCounter(&t);
  return 1000.0 * double(t.QuadPart) / double(f.QuadPart);
}

struct Find {
  const char *needle;
  HWND found;
};

static BOOL CALLBACK find_proc(HWND h, LPARAM l) {
  Find *f = reinterpret_cast<Find *>(l);
  char title[512];
  if (IsWindowVisible(h) && GetWindowTextA(h, title, sizeof(title)) > 0 && std::strstr(title, f->needle)) {
    f->found = h;
    return FALSE;
  }
  return TRUE;
}

static void save_bmp(const char *path, const void *bits, int w, int h) {
  BITMAPFILEHEADER fh = {};
  BITMAPINFOHEADER ih = {};
  ih.biSize = sizeof(ih);
  ih.biWidth = w;
  ih.biHeight = -h;
  ih.biPlanes = 1;
  ih.biBitCount = 32;
  const DWORD bytes = DWORD(w) * DWORD(h) * 4;
  fh.bfType = 0x4d42;
  fh.bfOffBits = sizeof(fh) + sizeof(ih);
  fh.bfSize = fh.bfOffBits + bytes;
  FILE *f = std::fopen(path, "wb");
  if (!f) return;
  std::fwrite(&fh, sizeof(fh), 1, f);
  std::fwrite(&ih, sizeof(ih), 1, f);
  std::fwrite(bits, bytes, 1, f);
  std::fclose(f);
}

int main(int argc, char **argv) {
  if (argc < 2) {
    std::printf("usage: hover_flicker_check TITLE [SECONDS [OUTDIR [SKIPTOP]]]\n");
    return 1;
  }
  const double seconds = argc > 2 ? std::atof(argv[2]) : 20.0;
  const std::string outdir = argc > 3 ? argv[3] : ".";
  const int skip_top = argc > 4 ? std::atoi(argv[4]) : 40;
  Find f = {argv[1], nullptr};
  for (int i = 0; i < 100 && !f.found; i++) {
    EnumWindows(find_proc, reinterpret_cast<LPARAM>(&f));
    if (!f.found) Sleep(100);
  }
  if (!f.found) {
    std::printf("[FAIL] no visible window with \"%s\" in its title\n", argv[1]);
    return 1;
  }
  HWND hwnd = f.found;
  SetForegroundWindow(hwnd);
  Sleep(300);
  RECT rc;
  GetClientRect(hwnd, &rc);
  const int w = rc.right, h = rc.bottom;
  POINT origin = {0, 0};
  ClientToScreen(hwnd, &origin);
  std::printf("hover_flicker_check: window %p, client %dx%d at (%ld,%ld), %.0f s\n", static_cast<void *>(hwnd), w, h,
              origin.x, origin.y, seconds);
  if (w < 64 || h < 64) {
    std::printf("[FAIL] client area too small\n");
    return 1;
  }
  POINT saved;
  GetCursorPos(&saved);

  std::atomic<bool> stop{false};
  std::thread mover([&] {
    // Zig-zag rows 40 px apart, 12 px steps, 4 ms apart.
    int x = 8, y = 8, dx = 12;
    while (!stop.load()) {
      SetCursorPos(origin.x + x, origin.y + y);
      x += dx;
      if (x >= w - 8 || x < 8) {
        dx = -dx;
        x += dx;
        y += 40;
        if (y >= h - 8) y = 8;
      }
      Sleep(4);
    }
  });

  HDC screen = GetDC(nullptr);
  HDC mem = CreateCompatibleDC(screen);
  BITMAPINFO bi = {};
  bi.bmiHeader.biSize = sizeof(bi.bmiHeader);
  bi.bmiHeader.biWidth = w;
  bi.bmiHeader.biHeight = -h;
  bi.bmiHeader.biPlanes = 1;
  bi.bmiHeader.biBitCount = 32;
  void *bits = nullptr;
  HBITMAP dib = CreateDIBSection(mem, &bi, DIB_RGB_COLORS, &bits, nullptr, 0);
  SelectObject(mem, dib);

  unsigned long long captures = 0, black_caps = 0, saved_n = 0;
  unsigned max_black = 0;
  bool sampled = false;
  unsigned base = ~0u;
  const double t0 = now_ms();
  double next = t0 + 1000.0;
  while (now_ms() - t0 < seconds * 1000.0) {
    if (!BitBlt(mem, 0, 0, w, h, screen, origin.x, origin.y, SRCCOPY)) {
      Sleep(2);
      continue;
    }
    captures++;
    unsigned black = 0, bx = 0, by = 0;
    const UINT32 *px = static_cast<const UINT32 *>(bits);
    for (int y = skip_top + 2; y < h; y += 4)
      for (int x = 2; x < w; x += 4) {
        const UINT32 p = px[size_t(y) * w + x];
        if ((p & 0xff) < 8 && ((p >> 8) & 0xff) < 8 && ((p >> 16) & 0xff) < 8) {
          if (!black) bx = x, by = y;
          black++;
        }
      }
    if (!sampled && now_ms() - t0 > seconds * 500.0) {
      // One ordinary capture from the middle of the run, to see what was checked.
      sampled = true;
      save_bmp((outdir + "\\hover-sample.bmp").c_str(), bits, w, h);
    }
    if (black < base) base = black;
    if (black >= base + kMinBlack) {
      if (black_caps < 20)
        std::printf("[BLACK] capture %llu at %.0f ms: %u black samples (fewest %u), first at (%u,%u)\n", captures,
                    now_ms() - t0, black, base, bx, by);
      if (saved_n < 4) {
        char path[512];
        std::snprintf(path, sizeof(path), "%s\\hover-black-%llu.bmp", outdir.c_str(), saved_n++);
        save_bmp(path, bits, w, h);
      }
      black_caps++;
      if (black > max_black) max_black = black;
    }
    if (now_ms() >= next) {
      next += 1000.0;
      std::printf("t=%3.0fs captures %llu black %llu\n", (now_ms() - t0) / 1000.0, captures, black_caps);
      std::fflush(stdout);
    }
  }
  stop.store(true);
  mover.join();
  SetCursorPos(saved.x, saved.y);
  std::printf("%s: captures %llu black %llu (largest %u samples, fewest %u)\n", black_caps ? "[FAIL]" : "[PASS]",
              captures, black_caps, max_black, base);
  DeleteObject(dib);
  DeleteDC(mem);
  ReleaseDC(nullptr, screen);
  return black_caps ? 2 : 0;
}
