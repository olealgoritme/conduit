// The DwmIcd=nvk crash-loop guard's decision (docs/dwm-on-nvk.md 4.1, "Failure safety"), kept
// free of Windows headers so the host test (bridge_dwm_guard_test.cpp, run by CI on Linux) runs
// the same code the UMD does. bridge_icd_backend.cpp does the file, registry and clock I/O.
//
// %ProgramData%\Helios\dwm-nvk-starts.txt holds the DWM starts on NVK that have not yet proved
// healthy, one per line: "<time> <pid> <created> <build>" (decimal; FILETIME ticks for the two
// times). A start counts against the guard only while all of these hold:
//  * it was made by the same UMD build (a driver update starts a new history: the restarts the
//    update itself causes, and any failures of the build it replaced, do not count);
//  * it was made by another process (pid and creation time name one process: a DWM that loads a
//    second copy of the UMD, as during a live driver update, is not a restart);
//  * it lies inside the window and not in the future.
// A DWM on NVK that is still presenting `healthy` seconds after its choice clears the history
// (healthy_due), so an ordinary restart, a live update or a reboot after one never adds up to a
// crash loop. When max_starts pending starts count, the next start goes to Venus and is not
// recorded, so NVK is tried again once they age out.
#pragma once

#include <cstddef>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>

namespace helios_bridge {
namespace dwm_guard {

constexpr std::size_t kMaxStarts = 16;
constexpr std::uint64_t kTicksPerSecond = 10000000ull;  // FILETIME: 100 ns

// One DWM start that chose NVK.
struct Start {
  std::uint64_t time = 0;     // the choice (FILETIME)
  std::uint32_t pid = 0;      // the dwm.exe process ...
  std::uint64_t created = 0;  // ... and its creation time: pids repeat, across boots too
  std::uint64_t build = 0;    // the UMD build (build_id); 0 in files older than this format
};

struct Params {
  std::uint32_t max_starts = 2;              // DwmNvkMaxStarts; 0 = no guard
  std::uint64_t window = 600 * kTicksPerSecond;  // DwmNvkGuardSeconds
};

struct Verdict {
  bool nvk = true;           // false: this start goes to Venus
  std::uint32_t counted = 0;  // pending starts that counted against the guard
  bool write = false;        // rewrite the file with `kept`
  Start kept[kMaxStarts];
  std::size_t kept_len = 0;
};

inline bool same_process(const Start& a, const Start& b) {
  return a.pid == b.pid && a.created == b.created;
}

// Does `s` (from the file) count against the start `self` is making now?
inline bool counts(const Start& s, const Start& self, const Params& p) {
  return s.build == self.build && !same_process(s, self) && s.time <= self.time &&
         self.time - s.time < p.window;
}

inline Verdict decide(const Start* history, std::size_t len, const Start& self, const Params& p) {
  Verdict v;
  if (p.max_starts == 0)
    return v;
  for (std::size_t i = 0; i < len; i++) {
    if (!counts(history[i], self, p))
      continue;
    v.counted++;
    if (v.kept_len == kMaxStarts - 1) {  // keep the newest, room for `self`
      for (std::size_t j = 1; j < v.kept_len; j++)
        v.kept[j - 1] = v.kept[j];
      v.kept_len--;
    }
    v.kept[v.kept_len++] = history[i];
  }
  if (v.counted >= p.max_starts) {
    v.nvk = false;
    return v;
  }
  v.kept[v.kept_len++] = self;
  v.write = true;
  return v;
}

// Has the DWM that chose NVK at `chose_at` been up long enough to clear the history?
inline bool healthy_due(std::uint64_t chose_at, std::uint64_t now, std::uint64_t healthy) {
  return now >= chose_at && now - chose_at >= healthy;
}

// Parses the file. A line with only a time (the format before builds and processes were kept)
// reads as build 0, which no current build matches, so it never counts.
inline std::size_t parse(const char* text, Start* out, std::size_t cap) {
  std::size_t n = 0;
  const char* p = text;
  while (*p && n < cap) {
    const char* eol = std::strchr(p, '\n');
    if (!eol)
      eol = p + std::strlen(p);
    std::uint64_t f[4] = {};
    int got = 0;
    bool bad = false;
    for (const char* q = p; q < eol;) {
      if (*q == ' ' || *q == '\t' || *q == '\r') {
        q++;
        continue;
      }
      char* end = nullptr;
      const unsigned long long x = std::strtoull(q, &end, 10);
      if (end == q || end > eol) {  // not a number: a damaged line never counts
        bad = true;
        break;
      }
      if (got < 4)
        f[got] = x;
      got++;
      q = end;
    }
    p = *eol ? eol + 1 : eol;
    if (bad || got == 0)
      continue;
    Start s;
    s.time = f[0];
    s.pid = static_cast<std::uint32_t>(f[1]);
    s.created = f[2];
    s.build = f[3];
    out[n++] = s;
  }
  return n;
}

// Writes `len` starts in the file's format. Returns the length, or 0 if `cap` is too small.
inline std::size_t format(const Start* starts, std::size_t len, char* out, std::size_t cap) {
  std::size_t used = 0;
  if (cap)
    out[0] = 0;
  for (std::size_t i = 0; i < len; i++) {
    const int w = std::snprintf(out + used, cap - used, "%llu %lu %llu %llu\n",
                                static_cast<unsigned long long>(starts[i].time),
                                static_cast<unsigned long>(starts[i].pid),
                                static_cast<unsigned long long>(starts[i].created),
                                static_cast<unsigned long long>(starts[i].build));
    if (w < 0 || static_cast<std::size_t>(w) >= cap - used)
      return 0;
    used += static_cast<std::size_t>(w);
  }
  return used;
}

// A build id from the UMD's own directory: every driver package lands in its own driver-store
// folder (helios_kmd_render.inf_amd64_<hash>), so a new package is a new id. FNV-1a over the
// path's UTF-16 units, ASCII case folded; never 0.
template <typename Char>
inline std::uint64_t build_id(const Char* dir) {
  std::uint64_t h = 1469598103934665603ull;
  for (; *dir; dir++) {
    std::uint32_t c = static_cast<std::uint32_t>(*dir) & 0xffffu;
    if (c >= 'A' && c <= 'Z')
      c += 'a' - 'A';
    h = (h ^ (c & 0xffu)) * 1099511628211ull;
    h = (h ^ (c >> 8)) * 1099511628211ull;
  }
  return h ? h : 1;
}

}  // namespace dwm_guard
}  // namespace helios_bridge
