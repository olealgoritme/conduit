// SPDX-License-Identifier: GPL-2.0
/*
 * Unit tests for nvgpu_escape.h, built and run on the build host:
 *
 *     make -C guest/linux check
 *
 * Reads host/backend/gen/fixtures/escape_sizes.tsv, which the Rust side
 * proves is the table the backend checks every call against and sends the
 * guest (abi::versions), and applies the guest's rule to each release in it:
 * every listed size is taken, any other size of that escape is refused, an
 * escape the release does not list is left to the backend.
 */
#include "../nvgpu_escape.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int failures;

#define CHECK(cond)                                                            \
  do {                                                                         \
    if (!(cond)) {                                                             \
      fprintf(stderr, "%s:%d: CHECK failed: %s\n", __FILE__, __LINE__, #cond); \
      failures++;                                                              \
    }                                                                          \
  } while (0)

#define MAX_RELEASES 16
#define MAX_ROWS 64

struct release {
  char name[32];
  struct nvgpu_escape_size sizes[NVGPU_MAX_ESCAPE_SIZES];
  int n;
};

static struct release releases[MAX_RELEASES];
static int num_releases;

static struct release *release_named(const char *name) {
  int i;

  for (i = 0; i < num_releases; i++)
    if (!strcmp(releases[i].name, name))
      return &releases[i];
  if (num_releases == MAX_RELEASES) {
    fprintf(stderr, "too many releases\n");
    exit(2);
  }
  snprintf(releases[num_releases].name, sizeof(releases[0].name), "%s", name);
  return &releases[num_releases++];
}

static void load(const char *path) {
  char line[256];
  FILE *f = fopen(path, "r");

  if (!f) {
    perror(path);
    exit(2);
  }
  if (!fgets(line, sizeof(line), f)) /* header */
    exit(2);
  while (fgets(line, sizeof(line), f)) {
    char *rel = strtok(line, "\t");
    char *esc = strtok(NULL, "\t");
    char *name = strtok(NULL, "\t");
    char *sizes = strtok(NULL, "\t\n");
    struct release *r;
    char *s, *save = NULL;

    if (!rel || !esc || !name || !sizes) {
      fprintf(stderr, "bad fixture line\n");
      exit(2);
    }
    r = release_named(rel);
    for (s = strtok_r(sizes, ",", &save); s; s = strtok_r(NULL, ",", &save)) {
      if (r->n == NVGPU_MAX_ESCAPE_SIZES) {
        fprintf(stderr, "%s has more sizes than the module holds\n", rel);
        exit(2);
      }
      r->sizes[r->n].escape = (u32)strtoul(esc, NULL, 16);
      r->sizes[r->n].size =
          strcmp(s, "any") ? (u32)strtoul(s, NULL, 10) : NVGPU_ESCAPE_SIZE_ANY;
      r->n++;
    }
  }
  fclose(f);
}

/* Every size the table lists is taken; the next one up and down is not. */
static void test_every_release(void) {
  int i, k;

  CHECK(num_releases >= 5);
  for (i = 0; i < num_releases; i++) {
    struct release *r = &releases[i];

    for (k = 0; k < r->n; k++) {
      u32 esc = r->sizes[k].escape, size = r->sizes[k].size;

      if (size == NVGPU_ESCAPE_SIZE_ANY) {
        CHECK(nvgpu_escape_size_ok(r->sizes, r->n, esc, 0));
        CHECK(nvgpu_escape_size_ok(r->sizes, r->n, esc, 2304));
        continue;
      }
      CHECK(nvgpu_escape_size_ok(r->sizes, r->n, esc, size));
      /* 4 apart is never another listed size of any escape here. */
      CHECK(!nvgpu_escape_size_ok(r->sizes, r->n, esc, size + 1));
      CHECK(!nvgpu_escape_size_ok(r->sizes, r->n, esc, size - 1));
    }
  }
}

/* The sizes that differ between releases, which a literal per handler gets
 * wrong for all but one of them. */
static void test_the_sizes_releases_disagree_on(void) {
  struct release *r535 = release_named("535.129.03");
  struct release *r565 = release_named("565.57.01");
  struct release *r580 = release_named("580.178.04");
  struct release *r610 = release_named("610.57.04");

  /* NVOS46/NVOS47: 535's userspace sends 40 for UNMAP_MEMORY_DMA, 565 and
   * later 48; MAP_MEMORY_DMA grew from 56 to 64 in 580. */
  CHECK(nvgpu_escape_size_ok(r535->sizes, r535->n, 0x58, 40));
  CHECK(!nvgpu_escape_size_ok(r535->sizes, r535->n, 0x58, 48));
  CHECK(nvgpu_escape_size_ok(r565->sizes, r565->n, 0x58, 48));
  CHECK(!nvgpu_escape_size_ok(r565->sizes, r565->n, 0x58, 40));
  CHECK(nvgpu_escape_size_ok(r565->sizes, r565->n, 0x57, 56));
  CHECK(!nvgpu_escape_size_ok(r565->sizes, r565->n, 0x57, 64));
  CHECK(nvgpu_escape_size_ok(r580->sizes, r580->n, 0x57, 64));
  CHECK(!nvgpu_escape_size_ok(r580->sizes, r580->n, 0x57, 56));
  /* EXPORT_TO_DMABUF_FD: 2600 until 565, 2608 from 580. */
  CHECK(nvgpu_escape_size_ok(r565->sizes, r565->n, 0xd9, 2600));
  CHECK(nvgpu_escape_size_ok(r610->sizes, r610->n, 0xd9, 2608));
  CHECK(!nvgpu_escape_size_ok(r610->sizes, r610->n, 0xd9, 2600));
}

/* The call that stopped NVML on 565.77: a 32-byte RM_ALLOC. */
static void test_rm_alloc_takes_both_sizes_everywhere(void) {
  int i;

  for (i = 0; i < num_releases; i++) {
    struct release *r = &releases[i];

    CHECK(nvgpu_escape_size_ok(r->sizes, r->n, 0x2b, 32));
    CHECK(nvgpu_escape_size_ok(r->sizes, r->n, 0x2b, 48));
    CHECK(!nvgpu_escape_size_ok(r->sizes, r->n, 0x2b, 40));
    CHECK(!nvgpu_escape_size_ok(r->sizes, r->n, 0x2b, 56));
  }
}

/* Nothing to apply is not a refusal, and neither is an escape nobody listed. */
static void test_what_is_not_refused(void) {
  struct release *r = release_named("565.57.01");

  CHECK(nvgpu_escape_size_ok(NULL, 0, 0x2b, 7));
  CHECK(nvgpu_escape_size_ok(r->sizes, r->n, 0xd1 /* STATUS_CODE */, 7));
}

static void put32(u8 *b, u32 at, u32 v) {
  b[at] = v & 0xff;
  b[at + 1] = v >> 8 & 0xff;
  b[at + 2] = v >> 16 & 0xff;
  b[at + 3] = v >> 24 & 0xff;
}

/*
 * The two RM_ALLOC layouts: paramsSize and status are not where the other
 * puts them. Reading NVOS21's at NVOS64's offsets reads pRightsRequested's
 * high word as the size, and writes the answer into padding.
 */
static void test_alloc_layouts(void) {
  const struct nvgpu_alloc_layout *l21 = nvgpu_alloc_layout(32);
  const struct nvgpu_alloc_layout *l64 = nvgpu_alloc_layout(48);
  u8 b[48];

  CHECK(l21 && l21->size == 32 && l21->params_size_at == 24 &&
        l21->status_at == 28);
  CHECK(l64 && l64->size == 48 && l64->params_size_at == 32 &&
        l64->status_at == 40);
  CHECK(nvgpu_alloc_layout(0) == NULL);
  CHECK(nvgpu_alloc_layout(28) == NULL);
  CHECK(nvgpu_alloc_layout(40) == NULL);
  CHECK(nvgpu_alloc_layout(56) == NULL);

  memset(b, 0, sizeof(b));
  put32(b, 24, 0x11223344);
  put32(b, 32, 0x55667788);
  CHECK(nvgpu_le32_at(b, l21->params_size_at) == 0x11223344);
  CHECK(nvgpu_le32_at(b, l64->params_size_at) == 0x55667788);
}

int main(int argc, char **argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s escape_sizes.tsv\n", argv[0]);
    return 2;
  }
  load(argv[1]);
  test_every_release();
  test_the_sizes_releases_disagree_on();
  test_rm_alloc_takes_both_sizes_everywhere();
  test_what_is_not_refused();
  test_alloc_layouts();
  if (failures) {
    fprintf(stderr, "%d check(s) failed\n", failures);
    return 1;
  }
  printf("escape_test: ok (%d releases)\n", num_releases);
  return 0;
}
