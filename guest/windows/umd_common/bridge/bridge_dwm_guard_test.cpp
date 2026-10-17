// Host test for bridge_dwm_guard.h, the DwmIcd=nvk crash-loop guard's decision.
//
//   c++ -std=c++17 -Wall -Wextra -Werror -o dwm_guard_test
//       guest/windows/umd_common/bridge/bridge_dwm_guard_test.cpp && ./dwm_guard_test
//
// CI runs it on Linux (.github/workflows/ci.yml, job windows-guest-host).
#include "bridge_dwm_guard.h"

#include <cstdio>
#include <cstring>

using namespace helios_bridge::dwm_guard;

static int failures;
#define CHECK(cond)                                                       \
  do {                                                                    \
    if (!(cond)) {                                                        \
      std::fprintf(stderr, "FAIL %s:%d: %s\n", __FILE__, __LINE__, #cond); \
      failures++;                                                         \
    }                                                                     \
  } while (0)

static constexpr std::uint64_t S = kTicksPerSecond;
static constexpr std::uint64_t T0 = 134000000000000000ull;  // some FILETIME in 2025
static constexpr std::uint64_t kBuildA = 0x1111, kBuildB = 0x2222;

static Start start(std::uint64_t t, std::uint32_t pid, std::uint64_t build = kBuildA) {
  Start s;
  s.time = t;
  s.pid = pid;
  s.created = t - S;  // a process started a second before its choice
  s.build = build;
  return s;
}

// The DWM history a test builds up: runs `self` through decide() and applies the verdict the
// way bridge_icd_backend.cpp does (write the kept list when told to).
struct File {
  Start starts[kMaxStarts];
  std::size_t len = 0;
  Params p;

  bool dwm_start(const Start& self) {
    const Verdict v = decide(starts, len, self, p);
    if (v.write) {
      std::memcpy(starts, v.kept, sizeof(Start) * v.kept_len);
      len = v.kept_len;
    }
    return v.nvk;
  }
  void healthy() { len = 0; }  // a DWM on NVK reached healthy_due: the file goes
};

static void crash_loop_still_trips() {
  // A DWM that dies at every NVK start: Windows restarts it seconds apart.
  File f;
  CHECK(f.dwm_start(start(T0, 100)));
  CHECK(f.dwm_start(start(T0 + 3 * S, 101)));
  CHECK(!f.dwm_start(start(T0 + 6 * S, 102)));  // third start: Venus
  CHECK(f.len == 2);                            // not recorded
  CHECK(!f.dwm_start(start(T0 + 9 * S, 103)));
  // Once the first start leaves the window, NVK is tried again.
  CHECK(f.dwm_start(start(T0 + 601 * S, 104)));
}

static void crash_loop_across_reboots_trips() {
  // A start that takes the machine down (a bugcheck) never gets healthy, and the next boot's
  // DWM often gets the same pid: the creation time tells them apart, so they count.
  File f;
  Start a = start(T0, 1234);
  Start b = start(T0 + 40 * S, 1234);
  Start c = start(T0 + 80 * S, 1234);
  CHECK(f.dwm_start(a));
  CHECK(f.dwm_start(b));
  CHECK(!f.dwm_start(c));
}

static void live_update_then_reboot_stays_on_nvk() {
  // The 2026-10-13 case: DWM on NVK with build A for hours; a live driver update to build B
  // restarts DWM twice, and the machine reboots a few minutes later.
  File f;
  CHECK(f.dwm_start(start(T0, 500)));
  f.healthy();
  const std::uint64_t u = T0 + 3600 * S;
  CHECK(f.dwm_start(start(u, 600, kBuildB)));
  CHECK(f.dwm_start(start(u + 2 * S, 601, kBuildB)));
  f.healthy();  // the second DWM presents on NVK for DwmNvkHealthySeconds
  CHECK(f.len == 0);
  CHECK(f.dwm_start(start(u + 300 * S, 700, kBuildB)));  // after the reboot: NVK
}

static void reboot_before_healthy_falls_back_once() {
  // The same update, but the reboot comes before either DWM got healthy: both starts are
  // still pending, so the guard trips (bridge_icd_backend.cpp then checks that Venus can make
  // the device and stays on NVK when it cannot).
  File f;
  const std::uint64_t u = T0;
  CHECK(f.dwm_start(start(u, 600, kBuildB)));
  CHECK(f.dwm_start(start(u + 2 * S, 601, kBuildB)));
  CHECK(!f.dwm_start(start(u + 20 * S, 700, kBuildB)));
}

static void old_build_starts_do_not_count() {
  // Build A crash-looped and tripped the guard; the user installs build B within the window.
  File f;
  CHECK(f.dwm_start(start(T0, 100, kBuildA)));
  CHECK(f.dwm_start(start(T0 + 3 * S, 101, kBuildA)));
  CHECK(!f.dwm_start(start(T0 + 6 * S, 102, kBuildA)));
  CHECK(f.dwm_start(start(T0 + 60 * S, 200, kBuildB)));
  CHECK(f.len == 1 && f.starts[0].build == kBuildB);  // build A's starts are gone
}

static void same_process_is_not_a_restart() {
  // A live update loads the new UMD into the running DWM; each copy decides once.
  File f;
  Start a = start(T0, 300, kBuildA);
  CHECK(f.dwm_start(a));
  Start a2 = a;
  a2.time = T0 + 10 * S;
  a2.build = kBuildB;
  CHECK(f.dwm_start(a2));
  Start a3 = a2;
  a3.time = T0 + 20 * S;
  CHECK(f.dwm_start(a3));
  CHECK(f.len == 1);
  CHECK(f.dwm_start(start(T0 + 30 * S, 301, kBuildB)));
  CHECK(f.len == 2);
}

static void window_and_clock() {
  Params p;
  const Start self = start(T0, 9);
  CHECK(counts(start(T0 - 599 * S, 1), self, p));
  CHECK(!counts(start(T0 - 600 * S, 1), self, p));  // the window is half-open
  CHECK(!counts(start(T0 + S, 1), self, p));        // clock went back: ignored
  Start other_build = start(T0 - S, 1, kBuildB);
  CHECK(!counts(other_build, self, p));
}

static void guard_off_and_limits() {
  File f;
  f.p.max_starts = 0;
  for (std::uint32_t i = 0; i < 10; i++)
    CHECK(f.dwm_start(start(T0 + i * S, 100 + i)));
  CHECK(f.len == 0);  // nothing recorded with the guard off

  File g;
  g.p.max_starts = 100;  // more than the file keeps
  for (std::uint32_t i = 0; i < 40; i++)
    CHECK(g.dwm_start(start(T0 + i * S, 100 + i)));
  CHECK(g.len == kMaxStarts);
  CHECK(g.starts[kMaxStarts - 1].pid == 139);  // the newest are kept
  CHECK(g.starts[0].pid == 139 - (kMaxStarts - 1));
}

static void healthy() {
  CHECK(!healthy_due(T0, T0 + 29 * S, 30 * S));
  CHECK(healthy_due(T0, T0 + 30 * S, 30 * S));
  CHECK(healthy_due(T0, T0, 0));
  CHECK(!healthy_due(T0, T0 - S, 0));  // clock went back
}

static void file_format() {
  Start in[3] = {start(T0, 1), start(T0 + S, 4294967295u, kBuildB), start(T0 + 2 * S, 7, 0)};
  char buf[512];
  const std::size_t n = format(in, 3, buf, sizeof(buf));
  CHECK(n > 0 && n == std::strlen(buf));
  Start out[kMaxStarts];
  CHECK(parse(buf, out, kMaxStarts) == 3);
  for (int i = 0; i < 3; i++) {
    CHECK(out[i].time == in[i].time);
    CHECK(out[i].pid == in[i].pid);
    CHECK(out[i].created == in[i].created);
    CHECK(out[i].build == in[i].build);
  }
  CHECK(format(in, 3, buf, 20) == 0);  // too small

  // The previous format (a time per line) parses with build 0 and so never counts.
  const char* old_file = "134000000000000000\r\n134000000100000000\n";
  CHECK(parse(old_file, out, kMaxStarts) == 2);
  CHECK(out[1].time == 134000000100000000ull && out[1].build == 0 && out[1].pid == 0);
  CHECK(!counts(out[1], start(T0 + 20 * S, 5), Params()));

  // Damaged and empty lines are skipped; the cap holds.
  const char* damaged = "\n  \nabc 1 2 3\n5 6 7 8\n9 x\n10 11 12 13 14\n";
  CHECK(parse(damaged, out, kMaxStarts) == 2);
  CHECK(out[0].time == 5 && out[0].build == 8);
  CHECK(out[1].time == 10 && out[1].build == 13);
  CHECK(parse(damaged, out, 1) == 1);
  CHECK(parse("", out, kMaxStarts) == 0);
}

static void build_ids() {
  const std::uint64_t a = build_id(u"C:\\Windows\\System32\\DriverStore\\FileRepository\\helios.inf_amd64_0123\\");
  const std::uint64_t a_upper = build_id(u"C:\\WINDOWS\\System32\\DriverStore\\FileRepository\\HELIOS.inf_amd64_0123\\");
  const std::uint64_t b = build_id(u"C:\\Windows\\System32\\DriverStore\\FileRepository\\helios.inf_amd64_4567\\");
  CHECK(a == a_upper);
  CHECK(a != b);
  CHECK(a != 0 && build_id(u"") != 0);
  CHECK(build_id("abc") == build_id(u"abc"));  // narrow and UTF-16 agree on ASCII
}

int main() {
  crash_loop_still_trips();
  crash_loop_across_reboots_trips();
  live_update_then_reboot_stays_on_nvk();
  reboot_before_healthy_falls_back_once();
  old_build_starts_do_not_count();
  same_process_is_not_a_restart();
  window_and_clock();
  guard_off_and_limits();
  healthy();
  file_format();
  build_ids();
  if (failures) {
    std::fprintf(stderr, "dwm_guard_test: %d failures\n", failures);
    return 1;
  }
  std::printf("dwm_guard_test: ok\n");
  return 0;
}
