/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * test_present.c -- the present levels (nb_present.h).
 *
 * --present-mode parsing; where each mode starts and how far it falls; that
 * only a failure of the level in force moves it, one level at a time; the
 * GPU copy's buffer layouts from the compositor's tranches (8-bit only,
 * scanout-capable first in fullscreen, the v3 list without feedback); and
 * the release state machine with and without explicit sync.
 */
#include <stdio.h>
#include <stdint.h>
#include <stdlib.h>

#include "nb_present.h"

static int checks, fails;
#define CHECK(c, ...) do { checks++; if (!(c)) { fails++; \
        printf("FAIL %s:%d: ", __FILE__, __LINE__); printf(__VA_ARGS__); \
        printf("\n"); } } while (0)

#define XR24 NB_PC_FCC_XR24
#define AR24 NB_PC_FCC_AR24
#define XR30 0x30335258u        /* DRM_FORMAT_XRGB2101010 */
#define NV_BL 0x0300000000606014ull
#define NV_BL2 0x0300000000e08014ull
#define LIN 0ull

static void test_parse(void)
{
    static const struct { const char *s; int m; } ok[] = {
        { "auto", NB_PM_AUTO }, { "native", NB_PM_NATIVE },
        { "zero-copy", NB_PM_NATIVE }, { "linear", NB_PM_LINEAR },
        { "shm", NB_PM_SHM }, { "copy", NB_PM_COPY },
        { "gpu-copy", NB_PM_COPY }, { "readback", NB_PM_READBACK },
    };
    int m = -7;

    for (size_t i = 0; i < sizeof(ok) / sizeof(ok[0]); i++) {
        m = -7;
        CHECK(nb_pm_parse(ok[i].s, &m) == 0 && m == ok[i].m, "parse %s",
              ok[i].s);
    }
    m = -7;
    CHECK(nb_pm_parse("fast", &m) == -1 && m == -7, "unknown word refused");
    CHECK(nb_pm_parse("", &m) == -1, "empty refused");
    CHECK(nb_pm_parse(NULL, &m) == -1, "NULL refused");
    CHECK(nb_pm_parse("Copy", &m) == -1, "case matters, like the other flags");
    /* Names round-trip. */
    for (int k = 0; k < NB_PM_COUNT; k++) {
        CHECK(nb_pm_parse(nb_pm_name(k), &m) == 0 && m == k, "name %d", k);
    }
    /* The first four keep the old tier numbers. */
    CHECK(NB_PM_AUTO == 0 && NB_PM_NATIVE == 1 && NB_PM_LINEAR == 2 &&
          NB_PM_SHM == 3, "tier numbers kept");
}

static void test_cascade(void)
{
    struct nb_pc c;

    /* auto: zero-copy -> gpu-copy -> readback, one level per failure. */
    nb_pc_init(&c, NB_PM_AUTO);
    CHECK(c.level == NB_PL_ZC && !nb_pc_forces_copy(&c), "auto starts zc");
    CHECK(nb_pc_take_change(&c) && !nb_pc_take_change(&c),
          "the start is reported once");
    CHECK(nb_pc_fail(&c, NB_PL_ZC, "import refused") && c.level == NB_PL_COPY,
          "zc failure -> copy");
    CHECK(nb_pc_forces_copy(&c) && !nb_pc_forces_readback(&c), "copy flags");
    CHECK(!strcmp(c.why, "import refused"), "reason kept: %s", c.why);
    CHECK(nb_pc_take_change(&c), "the drop is reported");
    /* A late failure of the level already left changes nothing. */
    CHECK(!nb_pc_fail(&c, NB_PL_ZC, "late") && c.level == NB_PL_COPY,
          "stale zc failure ignored");
    CHECK(!nb_pc_take_change(&c), "and is not reported");
    CHECK(nb_pc_fail(&c, NB_PL_COPY, "buffers refused") &&
          c.level == NB_PL_READBACK, "copy failure -> readback");
    CHECK(nb_pc_forces_readback(&c), "readback flag");
    CHECK(!nb_pc_fail(&c, NB_PL_READBACK, "pass failed") &&
          c.level == NB_PL_READBACK, "nothing below readback");

    /* copy: starts at gpu-copy, may fall to readback. */
    nb_pc_init(&c, NB_PM_COPY);
    CHECK(c.level == NB_PL_COPY && nb_pc_forces_copy(&c), "copy starts copy");
    CHECK(!nb_pc_fail(&c, NB_PL_ZC, "x"), "zc failure means nothing here");
    CHECK(nb_pc_fail(&c, NB_PL_COPY, "x") && c.level == NB_PL_READBACK,
          "copy -> readback");

    /* readback and shm: readback, fixed. */
    nb_pc_init(&c, NB_PM_READBACK);
    CHECK(c.level == NB_PL_READBACK && !nb_pc_fail(&c, NB_PL_READBACK, "x"),
          "readback fixed");
    nb_pc_init(&c, NB_PM_SHM);
    CHECK(c.level == NB_PL_READBACK && nb_pc_copy_allowed(&c),
          "shm reads dma-buf frames back");

    /* native: zero-copy, never falls (per-frame copy is separate). */
    nb_pc_init(&c, NB_PM_NATIVE);
    CHECK(c.level == NB_PL_ZC && !nb_pc_fail(&c, NB_PL_ZC, "x") &&
          c.level == NB_PL_ZC, "native fixed at zc");
    CHECK(nb_pc_copy_allowed(&c), "native still copies unadvertised pairs");

    /* linear: no copy at all. */
    nb_pc_init(&c, NB_PM_LINEAR);
    CHECK(c.level == NB_PL_ZC && !nb_pc_copy_allowed(&c), "linear: no copy");
    CHECK(!nb_pc_fail(&c, NB_PL_ZC, "x"), "linear fixed");
}

/* No dma-buf at all (no protocol, no usable layout): straight to the bottom
 * the mode allows, and only the read-back level can go on. */
static void test_floor(void)
{
    struct nb_pc c;

    nb_pc_init(&c, NB_PM_AUTO);
    (void)nb_pc_take_change(&c);
    CHECK(nb_pc_floor(&c, "no zwp_linux_dmabuf_v1") &&
          c.level == NB_PL_READBACK, "auto floors to readback");
    CHECK(!strcmp(c.why, "no zwp_linux_dmabuf_v1"), "reason: %s", c.why);
    CHECK(nb_pc_take_change(&c) && !nb_pc_take_change(&c),
          "reported once, not once per level");
    nb_pc_init(&c, NB_PM_COPY);
    CHECK(nb_pc_floor(&c, "x") && c.level == NB_PL_READBACK, "copy floors");
    nb_pc_init(&c, NB_PM_READBACK);
    CHECK(nb_pc_floor(&c, "x"), "readback is already there");
    nb_pc_init(&c, NB_PM_SHM);
    CHECK(nb_pc_floor(&c, "x"), "shm is already there");
    nb_pc_init(&c, NB_PM_NATIVE);
    CHECK(!nb_pc_floor(&c, "x") && c.level == NB_PL_ZC,
          "native asked for dma-buf: the caller fails");
    nb_pc_init(&c, NB_PM_LINEAR);
    CHECK(!nb_pc_floor(&c, "x") && c.level == NB_PL_ZC, "linear too");
}

static void fmts(struct nb_so_fmts *f, const uint32_t *fc, const uint64_t *m,
                 unsigned n)
{
    memset(f, 0, sizeof(*f));
    for (unsigned i = 0; i < n; i++) {
        nb_so_fmts_add(f, fc[i], m[i]);
    }
}

static void test_targets(void)
{
    struct nb_so_fmts scan, render, v3;
    struct nb_pc_cand c;
    /* Hyprland 0.47 on NVIDIA at 10 bit: the scanout tranche has XR30 and
     * XR24 block-linear, the render tranche XR24/AR24 block-linear and no
     * LINEAR XR24. */
    static const uint32_t sf[] = { XR30, XR24, XR24 };
    static const uint64_t sm[] = { NV_BL, NV_BL, NV_BL2 };
    static const uint32_t rf[] = { XR30, AR24, XR24, XR24, AR24 };
    static const uint64_t rm[] = { NV_BL, NV_BL, NV_BL2, NV_BL, LIN };
    static const uint32_t vf[] = { XR24, AR24, XR24 };
    static const uint64_t vm[] = { LIN, LIN, NB_PC_MOD_INVALID };

    fmts(&scan, sf, sm, 3);
    fmts(&render, rf, rm, 5);
    fmts(&v3, vf, vm, 3);

    /* Windowed: the render tranche, XR24 group first, never XR30. */
    nb_pc_targets(&scan, false, &render, true, &v3, &c);
    CHECK(c.n == 4, "windowed: 4 candidates, got %u", c.n);
    CHECK(c.fourcc[0] == XR24 && c.mod[0] == NV_BL2 &&
          c.fourcc[1] == XR24 && c.mod[1] == NV_BL, "XR24 in tranche order");
    CHECK(c.fourcc[2] == AR24 && c.mod[2] == NV_BL &&
          c.fourcc[3] == AR24 && c.mod[3] == LIN, "then AR24");
    for (unsigned i = 0; i < c.n; i++) {
        CHECK(c.fourcc[i] != XR30, "no 10-bit target");
    }

    /* Fullscreen: the scanout pairs the render set also lists come first. */
    nb_pc_targets(&scan, true, &render, true, &v3, &c);
    CHECK(c.n == 4 && c.fourcc[0] == XR24 && c.mod[0] == NV_BL &&
          c.mod[1] == NV_BL2, "fullscreen: scanout XR24 first (%u)", c.n);

    /* A scanout pair the render set does not list is not used. */
    {
        static const uint32_t rf2[] = { XR24 };
        static const uint64_t rm2[] = { NV_BL2 };
        struct nb_so_fmts r2;

        fmts(&r2, rf2, rm2, 1);
        nb_pc_targets(&scan, true, &r2, true, &v3, &c);
        CHECK(c.n == 1 && c.mod[0] == NV_BL2, "scan intersect render");
    }

    /* No feedback: the v3 list, implicit modifier left out. */
    nb_pc_targets(NULL, true, NULL, false, &v3, &c);
    CHECK(c.n == 2 && c.fourcc[0] == XR24 && c.mod[0] == LIN &&
          c.fourcc[1] == AR24, "v3 list (%u)", c.n);

    /* Feedback with a render tranche but no 8-bit pair: nothing (GBM's own
     * choice is the caller's fallback), not the v3 list. */
    {
        static const uint32_t rf3[] = { XR30 };
        static const uint64_t rm3[] = { NV_BL };
        struct nb_so_fmts r3;

        fmts(&r3, rf3, rm3, 1);
        nb_pc_targets(&scan, false, &r3, true, &v3, &c);
        CHECK(c.n == 0, "no 8-bit render pair: no candidates");
    }

    /* Nothing at all. */
    nb_pc_targets(NULL, false, NULL, false, NULL, &c);
    CHECK(c.n == 0, "empty");
}

static void test_release(void)
{
    struct nb_rel r = { 0 };
    uint64_t p1, p2;

    /* Implicit sync: wl_buffer.release frees it. */
    CHECK(nb_rel_commit(&r, false) == 0 && r.held && !nb_rel_waiting(&r),
          "implicit commit");
    CHECK(!nb_rel_signaled(&r, 100), "a point means nothing without xs");
    CHECK(nb_rel_event(&r) && !r.held, "release event frees");
    CHECK(!nb_rel_event(&r), "a second event is not a second release");

    /* Explicit sync: only the release point counts, points grow. */
    p1 = nb_rel_commit(&r, true);
    CHECK(p1 == 1 && nb_rel_waiting(&r), "first xs point 1");
    CHECK(!nb_rel_event(&r) && r.held, "wl_buffer.release ignored under xs");
    CHECK(!nb_rel_signaled(&r, 0), "not yet");
    CHECK(nb_rel_signaled(&r, 1) && !r.held, "point 1 frees");
    CHECK(!nb_rel_signaled(&r, 1), "only once");

    /* Committed twice before the first release: the newer point decides. */
    p1 = nb_rel_commit(&r, true);
    p2 = nb_rel_commit(&r, true);
    CHECK(p2 == p1 + 1, "points grow");
    CHECK(!nb_rel_signaled(&r, p1), "the older point does not free it");
    CHECK(nb_rel_signaled(&r, p2 + 5), "a later value does");

    /* xs commit then implicit commit: the event counts again. */
    nb_rel_commit(&r, true);
    nb_rel_commit(&r, false);
    CHECK(!nb_rel_signaled(&r, 1000), "point ignored after an implicit commit");
    CHECK(nb_rel_event(&r), "event frees after an implicit commit");

    /* Destroyed while held: the owner is told; the counter survives so the
     * slot's timeline keeps growing for the next buffer. */
    p1 = nb_rel_commit(&r, true);
    CHECK(nb_rel_forget(&r) && !r.held && !nb_rel_waiting(&r),
          "forget while held");
    CHECK(!nb_rel_forget(&r), "forget when free");
    CHECK(nb_rel_commit(&r, true) == p1 + 1, "timeline keeps growing");
}

int main(void)
{
    test_parse();
    test_cascade();
    test_floor();
    test_targets();
    test_release();
    printf("test_present: %d checks, %d failures\n", checks, fails);
    return fails ? 1 : 0;
}
