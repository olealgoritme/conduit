/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * test_scanout.c -- the fullscreen path decision (nb_scanout.h).
 *
 * The rules (windowed, 1:1, no GPU pass, --gpu-scale=on, Sharp filter, no
 * presentation feedback) and the measurement against simulated compositors:
 * one that scans a viewport-scaled buffer out (zero-copy wins), one that
 * scans out only an output-size buffer (GPU wins), one that composites both
 * at vblank (zero-copy wins: no GPU work for nothing), one that tears only an
 * output-size buffer (GPU wins on latency), plus restarts on a layout change,
 * stale presents from an earlier probe, and the scanout-tranche lookup.
 */
#include <stdio.h>
#include <stdint.h>
#include <stdlib.h>

#include "nb_scanout.h"

static int checks, fails;
#define CHECK(c, ...) do { checks++; if (!(c)) { fails++; \
        printf("FAIL %s:%d: ", __FILE__, __LINE__); printf(__VA_ARGS__); \
        printf("\n"); } } while (0)

#define MS 1000000ull

static struct nb_so_in fs_scaled(void)
{
    return (struct nb_so_in){
        .fullscreen = true, .gs_ok = true, .tear_or_latch = true,
        .exact = false, .can_measure = true, .guest_scanout = -1,
    };
}

/* A compositor: what it does with a frame on each path. */
struct comp {
    bool zc_on_zc, zc_on_gpu;       /* scanned out directly          */
    uint64_t lat_zc, lat_gpu;       /* receive -> present, ns        */
};

/* Run frames through decide/presented until settled; return the path. */
static enum nb_so_path run(struct nb_so *s, const struct nb_so_in *in,
                           const struct comp *c, uint64_t key, uint64_t *now,
                           unsigned max_frames)
{
    for (unsigned i = 0; i < max_frames; i++) {
        enum nb_so_path p = nb_so_decide(s, in, key, *now);
        unsigned gen = s->gen;

        *now += 4166667ull;
        nb_so_presented(s, gen, p, p == NB_SO_ZC ? c->zc_on_zc : c->zc_on_gpu,
                        p == NB_SO_ZC ? c->lat_zc : c->lat_gpu);
        if (s->phase == NB_SO_SETTLED || s->phase == NB_SO_FIXED) {
            break;
        }
    }
    return nb_so_decide(s, in, key, *now);
}

static void test_rules(void)
{
    struct nb_so s;
    struct nb_so_in in;

    nb_so_init(&s);
    in = fs_scaled();
    in.fullscreen = false;
    CHECK(nb_so_decide(&s, &in, 1, 0) == NB_SO_ZC, "windowed is zero-copy");
    CHECK(s.phase == NB_SO_FIXED && strstr(s.why, "windowed"), "%s", s.why);

    in = fs_scaled();
    in.exact = true;
    CHECK(nb_so_decide(&s, &in, 1, 0) == NB_SO_ZC, "1:1 zero-copy");
    CHECK(s.phase == NB_SO_FIXED && strstr(s.why, "1:1"), "%s", s.why);
    in.guest_scanout = 1;
    CHECK(nb_so_decide(&s, &in, 1, 0) == NB_SO_ZC, "1:1 in tranche");

    in = fs_scaled();
    in.gs_ok = false;
    in.gs_off = "--gpu-scale=off";
    CHECK(nb_so_decide(&s, &in, 1, 0) == NB_SO_ZC, "no pass: zero-copy");
    CHECK(strstr(s.why, "--gpu-scale=off") != NULL, "%s", s.why);

    in = fs_scaled();
    in.gs_forced = true;
    CHECK(nb_so_decide(&s, &in, 1, 0) == NB_SO_GPU, "forced: GPU");
    in.tear_or_latch = false;
    CHECK(nb_so_decide(&s, &in, 1, 0) == NB_SO_ZC, "forced only tear/latch");
    in.tear_or_latch = true;
    in.exact = true;
    CHECK(nb_so_decide(&s, &in, 1, 0) == NB_SO_ZC, "forced but 1:1");

    in = fs_scaled();
    in.nearest_scaled = true;
    CHECK(nb_so_decide(&s, &in, 1, 0) == NB_SO_GPU, "sharp: GPU");
    CHECK(strstr(s.why, "Sharp") != NULL, "%s", s.why);

    in = fs_scaled();
    in.can_measure = false;
    CHECK(nb_so_decide(&s, &in, 1, 0) == NB_SO_GPU, "unmeasurable tear: GPU");
    in.tear_or_latch = false;
    CHECK(nb_so_decide(&s, &in, 1, 0) == NB_SO_ZC, "unmeasurable vsync: ZC");
}

static void test_measured(void)
{
    struct nb_so s;
    struct nb_so_in in = fs_scaled();
    uint64_t now = 1;
    enum nb_so_path p;

    /* A compositor that scales on a plane: zero-copy is scanned out. */
    struct comp plane_scaler = { true, true, 2 * MS, 2 * MS };
    nb_so_init(&s);
    p = run(&s, &in, &plane_scaler, 7, &now, 500);
    CHECK(p == NB_SO_ZC && s.phase == NB_SO_SETTLED, "plane scaler: ZC (%s)",
          s.why);
    CHECK(s.acc[NB_SO_GPU].n == 0, "GPU never tried when ZC scans out");

    /* Output-size buffers only: the GPU pass gets the plane. */
    struct comp primary_only = { false, true, 4 * MS, 2 * MS };
    nb_so_init(&s);
    p = run(&s, &in, &primary_only, 7, &now, 500);
    CHECK(p == NB_SO_GPU && s.phase == NB_SO_SETTLED, "primary only: GPU (%s)",
          s.why);

    /* Composites everything at vblank: no reason to pay for the pass. */
    struct comp composites = { false, false, 5 * MS, 5 * MS + 200000 };
    nb_so_init(&s);
    p = run(&s, &in, &composites, 7, &now, 500);
    CHECK(p == NB_SO_ZC, "composites both: ZC (%s)", s.why);

    /* Tears only one output-size surface: GPU is much sooner. */
    struct comp tears_primary = { false, false, 5 * MS, 1200000 };
    nb_so_init(&s);
    p = run(&s, &in, &tears_primary, 7, &now, 500);
    CHECK(p == NB_SO_GPU, "tears output-size only: GPU (%s)", s.why);
    CHECK(strstr(s.why, "sooner") != NULL, "%s", s.why);

    /* Within the margin: zero-copy. */
    struct comp close_call = { false, false, 2 * MS + 250000, 2 * MS };
    nb_so_init(&s);
    p = run(&s, &in, &close_call, 7, &now, 500);
    CHECK(p == NB_SO_ZC, "within the margin: ZC (%s)", s.why);

    /* Settled stays settled with the same key... */
    nb_so_init(&s);
    p = run(&s, &in, &primary_only, 7, &now, 500);
    unsigned gen = s.gen;
    for (int i = 0; i < 100; i++) {
        CHECK(nb_so_decide(&s, &in, 7, now + (uint64_t)i * MS) == NB_SO_GPU,
              "settled");
    }
    CHECK(s.gen == gen, "no re-probe without a layout change");
    /* ...and a new layout measures again, zero-copy first. */
    CHECK(nb_so_decide(&s, &in, 8, now) == NB_SO_ZC &&
          s.phase == NB_SO_PROBE_ZC, "new key restarts at zero-copy");

    /* A rule in between forgets the key: back to measuring afterwards. */
    nb_so_init(&s);
    run(&s, &in, &primary_only, 7, &now, 500);
    in.fullscreen = false;
    nb_so_decide(&s, &in, 7, now);
    in.fullscreen = true;
    CHECK(nb_so_decide(&s, &in, 7, now) == NB_SO_ZC &&
          s.phase == NB_SO_PROBE_ZC, "fullscreen again measures again");
}

static void test_probe_details(void)
{
    struct nb_so s;
    struct nb_so_in in = fs_scaled();
    uint64_t now = 1;

    /* Presents from an earlier probe, or of the other path, do not count. */
    nb_so_init(&s);
    nb_so_decide(&s, &in, 1, now);
    unsigned old = s.gen;
    nb_so_decide(&s, &in, 2, now);          /* layout changed: new probe */
    nb_so_presented(&s, old, NB_SO_ZC, true, MS);
    CHECK(s.acc[NB_SO_ZC].n == 0, "stale generation ignored");
    nb_so_presented(&s, s.gen, NB_SO_GPU, true, MS);
    CHECK(s.acc[NB_SO_GPU].n == 0, "other path ignored while probing ZC");

    /* Too few frames: keeps measuring, however long it takes... */
    nb_so_init(&s);
    nb_so_decide(&s, &in, 1, now);
    for (int i = 0; i < 3; i++) {
        nb_so_presented(&s, s.gen, NB_SO_ZC, false, 3 * MS);
    }
    nb_so_decide(&s, &in, 1, now + 10000 * MS);
    CHECK(s.phase == NB_SO_PROBE_ZC, "3 frames are not enough");
    /* ...but a few frames after the probe time are. */
    for (int i = 0; i < 3; i++) {
        nb_so_presented(&s, s.gen, NB_SO_ZC, false, 3 * MS);
    }
    nb_so_decide(&s, &in, 1, now + 10000 * MS);
    CHECK(s.phase == NB_SO_PROBE_GPU, "6 frames after the probe time move on");

    /* GPU pass never presented anything (it failed): zero-copy. */
    nb_so_init(&s);
    nb_so_decide(&s, &in, 1, now);
    for (unsigned i = 0; i < NB_SO_MIN_SAMPLES; i++) {
        nb_so_presented(&s, s.gen, NB_SO_ZC, false, 3 * MS);
    }
    nb_so_decide(&s, &in, 1, now);
    CHECK(s.phase == NB_SO_PROBE_GPU, "on to GPU");
    in.gs_ok = false;
    in.gs_off = "the GPU pass failed";
    CHECK(nb_so_decide(&s, &in, 1, now) == NB_SO_ZC, "pass gone: ZC");

    /* 1:1 outside the scanout tranche is measured; inside it is not. */
    in = fs_scaled();
    in.exact = true;
    in.guest_scanout = 0;
    nb_so_init(&s);
    CHECK(nb_so_decide(&s, &in, 1, now) == NB_SO_ZC &&
          s.phase == NB_SO_PROBE_ZC && strstr(s.why, "tranche"),
          "1:1 off-tranche measures (%s)", s.why);

    /* Change notification for logging. */
    nb_so_init(&s);
    in = fs_scaled();
    nb_so_decide(&s, &in, 1, now);
    CHECK(nb_so_take_change(&s), "first decision is a change");
    nb_so_decide(&s, &in, 1, now);
    CHECK(!nb_so_take_change(&s), "same decision is not");

    /* The median ignores outliers. */
    struct nb_so_acc a = {0};
    uint32_t v[] = { 900, 1000, 1100, 50000, 1000 };
    for (unsigned i = 0; i < 5; i++) {
        a.lat_us[a.k++] = v[i];
    }
    CHECK(nb_so_median_us(&a) == 1000, "median %u", nb_so_median_us(&a));
}

static void test_tranche(void)
{
    struct nb_so_fmts f = {0};
    uint64_t mods[8];
    const uint32_t XR24 = 0x34325258, AR24 = 0x34325241;

    CHECK(nb_so_guest_scanout(&f, false, XR24, AR24, 0) == -1, "unknown");
    nb_so_fmts_add(&f, XR24, 0x0300000000606014ull);
    nb_so_fmts_add(&f, XR24, 0);
    nb_so_fmts_add(&f, XR24, 0);            /* duplicate */
    nb_so_fmts_add(&f, AR24, 0x0300000000606015ull);
    CHECK(f.n == 3, "dedup %u", f.n);
    CHECK(nb_so_guest_scanout(&f, true, XR24, AR24, 0x0300000000606014ull) == 1,
          "listed");
    CHECK(nb_so_guest_scanout(&f, true, XR24, AR24, 0x0300000000606015ull) == 1,
          "the alpha twin counts");
    CHECK(nb_so_guest_scanout(&f, true, XR24, AR24, 0x0300000000606016ull) == 0,
          "not listed");
    CHECK(nb_so_fmts_mods(&f, XR24, mods, 8) == 2 && mods[1] == 0, "mods");
    for (unsigned i = 0; i < NB_SO_FMTS + 4; i++) {
        nb_so_fmts_add(&f, XR24, 0x100 + i);
    }
    CHECK(f.n == NB_SO_FMTS && f.overflowed, "bounded");

    uint64_t k1 = nb_so_mix(nb_so_mix(0, 5120), 1440);
    uint64_t k2 = nb_so_mix(nb_so_mix(0, 1440), 5120);
    CHECK(k1 != k2 && k1 == nb_so_mix(nb_so_mix(0, 5120), 1440), "key mix");
}

int main(void)
{
    test_rules();
    test_measured();
    test_probe_details();
    test_tranche();
    printf("test_scanout: %d checks, %d failed\n", checks, fails);
    return fails ? 1 : 0;
}
