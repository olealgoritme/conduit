// SPDX-License-Identifier: GPL-2.0
/*
 * Unit tests for nvgpu_devinfo.h, built and run on the build host:
 *
 *     make -C guest/linux check
 */
#include "../nvgpu_devinfo.h"

#include <stdio.h>

static int failures;

#define CHECK(cond)                                                            \
  do {                                                                         \
    if (!(cond)) {                                                             \
      fprintf(stderr, "%s:%d: CHECK failed: %s\n", __FILE__, __LINE__, #cond); \
      failures++;                                                              \
    }                                                                          \
  } while (0)

static u32 word(const u8 *p, u32 at) {
  return p[at] | (p[at + 1] << 8) | (p[at + 2] << 16) | ((u32)p[at + 3] << 24);
}

/* A record whose every word is its own index + 1, so a misplaced word shows. */
static struct nvgpu_devinfo record(void) {
  struct nvgpu_devinfo d;
  int i;

  for (i = 0; i < NVGPU_DI_FIELDS; i++)
    d.v[i] = i + 1;
  return d;
}

/* The three layouts a caller can have, found by the size its ioctl carries. */
static void test_sizes(void) {
  CHECK(nvgpu_devinfo_layout_for_size(20) == &nvgpu_devinfo_v535_129_03);
  CHECK(nvgpu_devinfo_layout_for_size(32) == &nvgpu_devinfo_v565_77_00);
  CHECK(nvgpu_devinfo_layout_for_size(36) == &nvgpu_devinfo_v580_178_04);
  CHECK(nvgpu_devinfo_layout_for_size(0) == NULL);
  CHECK(nvgpu_devinfo_layout_for_size(24) == NULL);
  CHECK(nvgpu_devinfo_layout_for_size(40) == NULL);
}

/* 36 bytes is the wire record, word for word: what every 575+ release did
 * before this existed must be what it still does. */
static void test_nine_words_are_the_wire_record(void) {
  const struct nvgpu_devinfo_layout *l = nvgpu_devinfo_layout_for_size(36);
  struct nvgpu_devinfo d = record();
  u8 out[36];
  int i;

  memset(out, 0, sizeof(out));
  CHECK(nvgpu_devinfo_encode(l, &d, out, 36) == 36);
  for (i = 0; i < 9; i++)
    CHECK(word(out, 4 * i) == (u32)i + 1);
}

/* A 565 caller: eight words, no mig_device, so everything after gpu_id is one
 * word nearer the front than in the record -- and nothing past 32 bytes. */
static void test_a_565_caller_gets_32_bytes(void) {
  const struct nvgpu_devinfo_layout *l = nvgpu_devinfo_layout_for_size(32);
  struct nvgpu_devinfo d = record();
  u8 buf[32 + 8];

  memset(buf, 0xAA, sizeof(buf));
  CHECK(nvgpu_devinfo_encode(l, &d, buf, 32) == 32);
  CHECK(word(buf, 0) == d.v[NVGPU_DI_GPU_ID]);
  CHECK(word(buf, 4) == d.v[NVGPU_DI_PRIMARY_INDEX]);
  CHECK(word(buf, 8) == d.v[NVGPU_DI_SUPPORTS_ALLOC]);
  CHECK(word(buf, 12) == d.v[NVGPU_DI_GENERIC_PAGE_KIND]);
  CHECK(word(buf, 16) == d.v[NVGPU_DI_PAGE_KIND_GENERATION]);
  CHECK(word(buf, 20) == d.v[NVGPU_DI_SECTOR_LAYOUT]);
  CHECK(word(buf, 24) == d.v[NVGPU_DI_SUPPORTS_SYNC_FD]);
  CHECK(word(buf, 28) == d.v[NVGPU_DI_SUPPORTS_SEMSURF]);
  for (int i = 32; i < (int)sizeof(buf); i++)
    CHECK(buf[i] == 0xAA); /* the old handler wrote 36 bytes here */
}

/* A 535 caller has no capability words at all. */
static void test_a_535_caller_gets_20_bytes(void) {
  const struct nvgpu_devinfo_layout *l = nvgpu_devinfo_layout_for_size(20);
  struct nvgpu_devinfo d = record();
  u8 buf[20 + 8];

  memset(buf, 0xAA, sizeof(buf));
  CHECK(nvgpu_devinfo_encode(l, &d, buf, 20) == 20);
  CHECK(word(buf, 0) == d.v[NVGPU_DI_GPU_ID]);
  CHECK(word(buf, 4) == d.v[NVGPU_DI_PRIMARY_INDEX]);
  CHECK(word(buf, 8) == d.v[NVGPU_DI_GENERIC_PAGE_KIND]);
  CHECK(word(buf, 12) == d.v[NVGPU_DI_PAGE_KIND_GENERATION]);
  CHECK(word(buf, 16) == d.v[NVGPU_DI_SECTOR_LAYOUT]);
  for (int i = 20; i < (int)sizeof(buf); i++)
    CHECK(buf[i] == 0xAA);
}

/* The bound holds even when the layout is bigger than the buffer: a field that
 * does not fit whole is dropped, never half written. */
static void test_the_bound_holds_for_a_short_buffer(void) {
  const struct nvgpu_devinfo_layout *l = nvgpu_devinfo_layout_for_size(36);
  struct nvgpu_devinfo d = record();
  u8 buf[30];

  memset(buf, 0xAA, sizeof(buf));
  nvgpu_devinfo_encode(l, &d, buf, sizeof(buf));
  CHECK(word(buf, 24) == d.v[NVGPU_DI_SECTOR_LAYOUT]);
  CHECK(buf[28] == 0xAA && buf[29] == 0xAA);
}

/* Layouts of one size agree, which is what lets a size name one. */
static void test_a_size_names_one_layout(void) {
  size_t i, j;

  for (i = 0; i < ARRAY_SIZE(nvgpu_devinfo_layouts); i++)
    for (j = 0; j < ARRAY_SIZE(nvgpu_devinfo_layouts); j++)
      if (nvgpu_devinfo_layouts[i]->size == nvgpu_devinfo_layouts[j]->size)
        CHECK(!memcmp(nvgpu_devinfo_layouts[i], nvgpu_devinfo_layouts[j],
                      sizeof(struct nvgpu_devinfo_layout)));
}

int main(void) {
  test_sizes();
  test_nine_words_are_the_wire_record();
  test_a_565_caller_gets_32_bytes();
  test_a_535_caller_gets_20_bytes();
  test_the_bound_holds_for_a_short_buffer();
  test_a_size_names_one_layout();
  if (failures) {
    fprintf(stderr, "%d failure(s)\n", failures);
    return 1;
  }
  puts("devinfo_test: ok");
  return 0;
}
