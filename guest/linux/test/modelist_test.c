// SPDX-License-Identifier: GPL-2.0
/*
 * Unit tests for nvgpu_modelist.h, built and run on the build host:
 *
 *     make -C guest/linux check
 */
#include "../nvgpu_modelist.h"

#include <stdio.h>
#include <string.h>

static int failures;

#define CHECK(cond)                                                            \
  do {                                                                         \
    if (!(cond)) {                                                             \
      fprintf(stderr, "%s:%d: CHECK failed: %s\n", __FILE__, __LINE__, #cond); \
      failures++;                                                              \
    }                                                                          \
  } while (0)

static void put(u8 *p, u32 v) {
  p[0] = v & 0xff;
  p[1] = (v >> 8) & 0xff;
  p[2] = (v >> 16) & 0xff;
  p[3] = v >> 24;
}

/* A payload as protocol::encode_display_mode_list writes it (no header). */
static unsigned int payload(u8 *b, u32 scanout, u32 count, u32 mhz,
                            const u32 (*m)[2], unsigned int n) {
  unsigned int i;

  put(b, scanout);
  put(b + 4, count);
  put(b + 8, mhz);
  put(b + 12, 0);
  for (i = 0; i < n; i++) {
    put(b + 16 + 8 * i, m[i][0]);
    put(b + 20 + 8 * i, m[i][1]);
  }
  return 16 + 8 * n;
}

static bool parse(const u8 *b, unsigned int len, struct nvgpu_modelist *l) {
  return nvgpu_modelist_parse(b, len, 64, 16384, 1000, l);
}

static void test_ok(void) {
  static const u32 m[][2] = {{5120, 1440}, {3840, 1080}, {1920, 1080}};
  u8 b[512];
  struct nvgpu_modelist l;
  unsigned int n = payload(b, 0, 3, 240000, m, 3);

  CHECK(parse(b, n, &l));
  CHECK(l.count == 3 && l.refresh_mhz == 240000);
  CHECK(l.w[0] == 5120 && l.h[0] == 1440);
  CHECK(l.w[2] == 1920 && l.h[2] == 1080);
  /* A longer buffer than needed is fine. */
  CHECK(parse(b, sizeof(b), &l) && l.count == 3);
}

static void test_refused(void) {
  static const u32 m[][2] = {{1920, 1080}};
  static const u32 bad[][2] = {{10, 10}, {99999, 1080}};
  u8 b[512];
  struct nvgpu_modelist l = {.count = 7};
  unsigned int n;

  n = payload(b, 1, 1, 60000, m, 1);
  CHECK(!parse(b, n, &l) && l.count == 7); /* another scanout */
  n = payload(b, 0, 0, 60000, m, 0);
  CHECK(!parse(b, n, &l)); /* empty */
  n = payload(b, 0, 2, 60000, m, 1);
  CHECK(!parse(b, n, &l)); /* shorter than its count */
  n = payload(b, 0, NVGPU_MODELIST_MAX + 1, 60000, m, 1);
  CHECK(!parse(b, sizeof(b), &l)); /* over the cap */
  CHECK(!parse(b, 15, &l));        /* no header */
  n = payload(b, 0, 2, 60000, bad, 2);
  CHECK(!parse(b, n, &l) && l.count == 7); /* nothing usable */
}

static void test_filtered(void) {
  static const u32 m[][2] = {
      {2560, 1440}, {10, 10}, {1920, 1080}, {2560, 1440}, {1280, 960}};
  u8 b[512];
  struct nvgpu_modelist l;
  unsigned int n = payload(b, 0, 5, 5000000, m, 5);

  CHECK(parse(b, n, &l));
  CHECK(l.count == 3);
  CHECK(l.w[1] == 1920 && l.w[2] == 1280);
  CHECK(l.refresh_mhz == 0); /* 5 kHz: out of range, keep the head's */
}

static void test_equal(void) {
  static const u32 a[][2] = {{1920, 1080}, {1280, 720}};
  static const u32 c[][2] = {{1920, 1080}, {1280, 960}};
  u8 b[256];
  struct nvgpu_modelist x, y;

  CHECK(parse(b, payload(b, 0, 2, 60000, a, 2), &x));
  CHECK(parse(b, payload(b, 0, 2, 60000, a, 2), &y));
  CHECK(nvgpu_modelist_equal(&x, &y));
  CHECK(parse(b, payload(b, 0, 2, 60000, c, 2), &y));
  CHECK(!nvgpu_modelist_equal(&x, &y));
  CHECK(parse(b, payload(b, 0, 2, 120000, a, 2), &y));
  CHECK(!nvgpu_modelist_equal(&x, &y));
}

int main(void) {
  test_ok();
  test_refused();
  test_filtered();
  test_equal();
  if (failures) {
    fprintf(stderr, "modelist_test: %d failure(s)\n", failures);
    return 1;
  }
  printf("modelist_test: ok\n");
  return 0;
}
