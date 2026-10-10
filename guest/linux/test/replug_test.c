// SPDX-License-Identifier: GPL-2.0
/*
 * Unit tests for nvgpu_replug.h, built and run on the build host:
 *
 *     make -C guest/linux check
 */
#include "../nvgpu_replug.h"

#include <stdio.h>

static int failures;

#define CHECK(cond)                                                            \
  do {                                                                         \
    if (!(cond)) {                                                             \
      fprintf(stderr, "%s:%d: CHECK failed: %s\n", __FILE__, __LINE__, #cond); \
      failures++;                                                              \
    }                                                                          \
  } while (0)

static bool probe(struct nvgpu_replug *r, bool *kick) {
  return nvgpu_replug_on_probe(r, kick);
}

/* mutter/KWin: the head is on the new mode when the grace period ends. */
static void test_followed_needs_no_replug(void) {
  struct nvgpu_replug r = {0};
  bool kick;

  CHECK(nvgpu_replug_on_mode(&r));
  CHECK(r.stage == NVGPU_REPLUG_WATCH);
  CHECK(probe(&r, &kick) && !kick); /* the compositor's re-probe */
  CHECK(nvgpu_replug_on_timer(&r, true, true) == NVGPU_REPLUG_NOTHING);
  CHECK(r.stage == NVGPU_REPLUG_IDLE);
  CHECK(probe(&r, &kick) && !kick);
  /* A stray timer run does nothing. */
  CHECK(nvgpu_replug_on_timer(&r, true, false) == NVGPU_REPLUG_NOTHING);
}

/* Hyprland/Sway: the old mode stays, so the connector is replugged once. */
static void test_ignored_replugs_once(void) {
  struct nvgpu_replug r = {0};
  bool kick;

  CHECK(nvgpu_replug_on_mode(&r));
  CHECK(probe(&r, &kick) && !kick);
  CHECK(nvgpu_replug_on_timer(&r, true, false) == NVGPU_REPLUG_DISCONNECT);
  /* The master's probe sees the disconnect and kicks the reconnect... */
  CHECK(!probe(&r, &kick) && kick);
  /* ...only once; later probes before it still read disconnected. */
  CHECK(!probe(&r, &kick) && !kick);
  CHECK(nvgpu_replug_on_timer(&r, true, false) == NVGPU_REPLUG_RECONNECT);
  CHECK(probe(&r, &kick) && !kick);
  CHECK(r.stage == NVGPU_REPLUG_IDLE);
}

/* Nobody probes during the replug: the timeout reconnects anyway. */
static void test_reconnect_without_probe(void) {
  struct nvgpu_replug r = {0};
  bool kick;

  nvgpu_replug_on_mode(&r);
  CHECK(nvgpu_replug_on_timer(&r, true, false) == NVGPU_REPLUG_DISCONNECT);
  CHECK(nvgpu_replug_on_timer(&r, true, false) == NVGPU_REPLUG_RECONNECT);
  CHECK(probe(&r, &kick) && !kick);
}

/* A new host mode during a replug does not restart the watch. */
static void test_mode_during_replug(void) {
  struct nvgpu_replug r = {0};
  bool kick;

  nvgpu_replug_on_mode(&r);
  CHECK(nvgpu_replug_on_timer(&r, true, false) == NVGPU_REPLUG_DISCONNECT);
  CHECK(!nvgpu_replug_on_mode(&r));
  CHECK(r.stage == NVGPU_REPLUG_OFF);
  CHECK(!probe(&r, &kick) && kick);
  CHECK(nvgpu_replug_on_timer(&r, true, false) == NVGPU_REPLUG_RECONNECT);
  /* Back to normal: the next mode change is watched again. */
  CHECK(nvgpu_replug_on_mode(&r));
  CHECK(r.stage == NVGPU_REPLUG_WATCH);
}

/* A second mode change while watching just restarts the watch. */
static void test_mode_during_watch(void) {
  struct nvgpu_replug r = {0};

  CHECK(nvgpu_replug_on_mode(&r));
  CHECK(nvgpu_replug_on_mode(&r));
  CHECK(r.stage == NVGPU_REPLUG_WATCH);
  CHECK(nvgpu_replug_on_timer(&r, true, true) == NVGPU_REPLUG_NOTHING);
}

/* The replug's own probe state is fresh for every replug. */
static void test_second_replug_kicks_again(void) {
  struct nvgpu_replug r = {0};
  bool kick;

  for (int i = 0; i < 2; i++) {
    nvgpu_replug_on_mode(&r);
    CHECK(nvgpu_replug_on_timer(&r, true, false) == NVGPU_REPLUG_DISCONNECT);
    CHECK(!probe(&r, &kick) && kick);
    CHECK(nvgpu_replug_on_timer(&r, true, false) == NVGPU_REPLUG_RECONNECT);
  }
}

static void test_applied(void) {
  /* Exact, and the whole-hertz rounding of drm_mode_vrefresh(). */
  CHECK(nvgpu_replug_applied(1920, 1080, 240, 1920, 1080, 240000));
  CHECK(nvgpu_replug_applied(1920, 1080, 60, 1920, 1080, 59940));
  CHECK(nvgpu_replug_applied(1920, 1080, 144, 1920, 1080, 143856));
  /* Another size or rate is not. */
  CHECK(!nvgpu_replug_applied(5120, 1440, 240, 1920, 1080, 240000));
  CHECK(!nvgpu_replug_applied(1920, 1200, 240, 1920, 1080, 240000));
  CHECK(!nvgpu_replug_applied(1920, 1080, 60, 1920, 1080, 240000));
  /* The table's 60 Hz mode of the same size is not the preferred one. */
  CHECK(!nvgpu_replug_applied(1920, 1080, 60, 1920, 1080, 120000));
}

/*
 * The head is off (DPMS: a lock screen, an idle blank) when the grace period
 * ends. The compositor turns it back on at the mode it kept, so the watch
 * must survive until then rather than count the dark head as following.
 */
static void test_head_off_keeps_watch(void) {
  struct nvgpu_replug r = {0};
  bool kick;

  CHECK(nvgpu_replug_on_mode(&r));
  CHECK(nvgpu_replug_on_timer(&r, false, false) == NVGPU_REPLUG_NOTHING);
  CHECK(r.stage == NVGPU_REPLUG_WATCH);
  CHECK(nvgpu_replug_watching(&r));
  CHECK(probe(&r, &kick) && !kick);
  /* Lit again at the old mode: the restarted grace period ends in a replug. */
  CHECK(nvgpu_replug_on_timer(&r, true, false) == NVGPU_REPLUG_DISCONNECT);
  CHECK(!nvgpu_replug_watching(&r));
  CHECK(!probe(&r, &kick) && kick);
  CHECK(nvgpu_replug_on_timer(&r, true, false) == NVGPU_REPLUG_RECONNECT);
  CHECK(r.stage == NVGPU_REPLUG_IDLE);
}

/* Lit again at the new mode (mutter applied it while dark): nothing to do. */
static void test_head_off_then_followed(void) {
  struct nvgpu_replug r = {0};

  nvgpu_replug_on_mode(&r);
  CHECK(nvgpu_replug_on_timer(&r, false, false) == NVGPU_REPLUG_NOTHING);
  CHECK(nvgpu_replug_on_timer(&r, true, true) == NVGPU_REPLUG_NOTHING);
  CHECK(r.stage == NVGPU_REPLUG_IDLE);
  CHECK(!nvgpu_replug_watching(&r));
}

/* Another host mode while the head is dark re-arms the same watch. */
static void test_mode_while_head_off(void) {
  struct nvgpu_replug r = {0};

  nvgpu_replug_on_mode(&r);
  CHECK(nvgpu_replug_on_timer(&r, false, false) == NVGPU_REPLUG_NOTHING);
  CHECK(nvgpu_replug_on_mode(&r));
  CHECK(nvgpu_replug_on_timer(&r, false, false) == NVGPU_REPLUG_NOTHING);
  CHECK(nvgpu_replug_watching(&r));
  CHECK(nvgpu_replug_on_timer(&r, true, false) == NVGPU_REPLUG_DISCONNECT);
}

/* No watch, no replug: a head turned on with no host change is left alone. */
static void test_idle_head_on_is_left_alone(void) {
  struct nvgpu_replug r = {0};

  CHECK(!nvgpu_replug_watching(&r));
  CHECK(nvgpu_replug_on_timer(&r, true, false) == NVGPU_REPLUG_NOTHING);
  CHECK(nvgpu_replug_on_timer(&r, false, false) == NVGPU_REPLUG_NOTHING);
  CHECK(r.stage == NVGPU_REPLUG_IDLE);
}

int main(void) {
  test_followed_needs_no_replug();
  test_ignored_replugs_once();
  test_reconnect_without_probe();
  test_mode_during_replug();
  test_mode_during_watch();
  test_second_replug_kicks_again();
  test_applied();
  test_head_off_keeps_watch();
  test_head_off_then_followed();
  test_mode_while_head_off();
  test_idle_head_on_is_left_alone();
  if (failures) {
    fprintf(stderr, "replug_test: %d failure(s)\n", failures);
    return 1;
  }
  printf("replug_test: ok\n");
  return 0;
}
