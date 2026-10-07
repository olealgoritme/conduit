// SPDX-License-Identifier: GPL-2.0
/*
 * Unit tests for nvgpu_version.h, built and run on the build host:
 *
 *     make -C guest/linux check
 *
 * The guest receives the host driver release as a string. This reads the same
 * file the Rust parser of /proc/driver/nvidia/version is tested on and checks
 * that, for every line it accepts, the string the guest would be sent (the
 * last column) parses to the same release in C.
 */
#include "../nvgpu_version.h"

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

static bool is(const char *s, u32 maj, u32 min, u32 pat) {
  u32 a = 99, b = 99, c = 99;

  return nvgpu_parse_version(s, &a, &b, &c) && a == maj && b == min && c == pat;
}

static bool refused(const char *s) {
  u32 a = 7, b = 8, c = 9;

  /* A refusal leaves the outputs alone. */
  return !nvgpu_parse_version(s, &a, &b, &c) && a == 7 && b == 8 && c == 9;
}

/* Every release Conduit has tables for, and the two-part one. */
static void test_releases(void) {
  CHECK(is("535.129.03", 535, 129, 3));
  CHECK(is("565.77", 565, 77, 0));
  CHECK(is("565.77.00", 565, 77, 0));
  CHECK(is("580.178.04", 580, 178, 4));
  CHECK(is("595.71.05", 595, 71, 5));
  CHECK(is("595.104.02", 595, 104, 2));
  CHECK(is("610.57.04", 610, 57, 4));
  CHECK(is("615.71.09", 615, 71, 9));
  CHECK(is(" 565.77\n", 565, 77, 0));
}

/* What sscanf("%u.%u.%u") used to let through, and the Rust side never did. */
static void test_not_releases(void) {
  CHECK(refused(NULL));
  CHECK(refused(""));
  CHECK(refused("565"));
  CHECK(refused("565."));
  CHECK(refused("565.x"));
  CHECK(refused("565.77x"));
  CHECK(refused("565.77."));
  CHECK(refused("565.77.x"));
  CHECK(refused("565.77.1.2"));
  CHECK(refused("-565.77"));
  CHECK(refused("+565.77"));
  CHECK(refused("565 .77"));
  CHECK(refused("99999999999.1"));
}

/* The string column of each accepted line in the shared fixture. */
static void test_fixture(const char *path) {
  FILE *f = fopen(path, "r");
  char line[1024];
  int rows = 0;

  if (!f) {
    perror(path);
    failures++;
    return;
  }
  while (fgets(line, sizeof(line), f)) {
    char *tab, *want, *sp, *raw;
    char canon[64];
    u32 a, b, c;

    if (line[0] == '#')
      continue;
    line[strcspn(line, "\n")] = '\0';
    tab = strchr(line, '\t');
    if (!tab) {
      fprintf(stderr, "%s: row without a tab: %s\n", path, line);
      failures++;
      continue;
    }
    want = tab + 1;
    rows++;
    if (!strcmp(want, "none"))
      continue; /* no version line, so no string reaches the guest */
    /* "<canonical> <open|closed> <raw>" */
    sp = strchr(want, ' ');
    raw = sp ? strchr(sp + 1, ' ') : NULL;
    if (!raw) {
      fprintf(stderr, "%s: bad expectation: %s\n", path, want);
      failures++;
      continue;
    }
    *strchr(want, ' ') = '\0';
    raw++;
    if (!nvgpu_parse_version(raw, &a, &b, &c)) {
      fprintf(stderr, "%s: C refused %s\n", path, raw);
      failures++;
      continue;
    }
    snprintf(canon, sizeof(canon), "%u.%u.%02u", a, b, c);
    if (strcmp(canon, want)) {
      fprintf(stderr, "%s: C read %s as %s, want %s\n", path, raw, canon, want);
      failures++;
    }
  }
  fclose(f);
  CHECK(rows >= 10);
}

int main(int argc, char **argv) {
  test_releases();
  test_not_releases();
  test_fixture(argc > 1 ? argv[1]
                        : "../../host/backend/gen/fixtures/proc_version.tsv");
  if (failures) {
    fprintf(stderr, "%d failure(s)\n", failures);
    return 1;
  }
  puts("version_test: ok");
  return 0;
}
