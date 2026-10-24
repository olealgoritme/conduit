/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * test_fg.c -- frame-generation pacing (nb_fg.h): when generated pictures
 * are shown, at which phase, and when generation steps aside.
 *
 * Simulates the viewer's timer loop: guest frames arrive at a steady rate
 * (with jitter, a pause, a rate near the output's refresh), the timer fires
 * when nb_fg_due() says, and a picture is "shown" unless the test says no
 * buffer is free.
 */
#include <math.h>
#include <stdio.h>
#include <stdint.h>

#include "nb_fg.h"

static int checks, fails;
#define CHECK(c, ...) do { checks++; if (!(c)) { fails++; \
        printf("FAIL %s:%d: ", __FILE__, __LINE__); printf(__VA_ARGS__); \
        printf("\n"); } } while (0)

#define MS 1000000ull

static bool near(float a, float b)
{
    return fabsf(a - b) < 1e-4f;
}

/* Run the timer for one guest interval after an arrival at `t0`; returns
 * how many pictures were shown and their phases in ph[]. */
static int run_cycle(struct nb_fg *f, uint64_t t0, uint64_t interval,
                     float *ph, uint64_t *at)
{
    int n = 0;
    uint64_t due;

    while ((due = nb_fg_due(f)) && due < t0 + interval && n < 8) {
        int step = 0;
        float t = nb_fg_peek(f, due, &step);

        if (t <= 0.0f) {
            break;
        }
        ph[n] = t;
        at[n] = due;
        n++;
        nb_fg_shown(f, step);
    }
    return n;
}

static void test_2x_steady(void)
{
    struct nb_fg f;
    uint64_t t = 1000 * MS, I = 16666667ull;   /* 60 fps */
    float ph[8];
    uint64_t at[8];
    float first;
    int n;

    nb_fg_init(&f, 2);
    nb_fg_set_refresh(&f, 4166667ull);        /* 240 Hz */
    first = nb_fg_arrival(&f, t, false);
    CHECK(near(first, 1.0f), "first frame: no pair, shown as it is");
    CHECK(nb_fg_due(&f) == 0, "nothing scheduled without a pair");
    for (int i = 1; i < 20; i++) {
        t += I;
        first = nb_fg_arrival(&f, t, true);
        n = run_cycle(&f, t, I, ph, at);
        if (i < 2) {
            continue;           /* the interval is learned from deltas */
        }
        CHECK(near(first, 0.5f), "2x: halfway picture at arrival (%f)", first);
        CHECK(n == 1 && near(ph[0], 1.0f), "2x: then the real frame (%d)", n);
        CHECK(n == 1 && at[0] >= t + I / 2 - MS / 10 && at[0] <= t + I / 2 + MS / 10,
              "2x: half a frame later (%llu)",
              (unsigned long long)(n ? at[0] - t : 0));
    }
    CHECK(f.interval > I - MS / 10 && f.interval < I + MS / 10,
          "interval measured (%llu)", (unsigned long long)f.interval);
}

static void test_3x_phases(void)
{
    struct nb_fg f;
    uint64_t t = 1000 * MS, I = 25 * MS;      /* 40 fps */
    float ph[8], first = 0;
    uint64_t at[8];
    int n = 0;

    nb_fg_init(&f, 3);
    nb_fg_set_refresh(&f, 6944444ull);        /* 144 Hz */
    nb_fg_arrival(&f, t, false);
    for (int i = 0; i < 10; i++) {
        t += I;
        first = nb_fg_arrival(&f, t, true);
        n = run_cycle(&f, t, I, ph, at);
    }
    CHECK(near(first, 1.0f / 3.0f), "3x: a third at arrival (%f)", first);
    CHECK(n == 2 && near(ph[0], 2.0f / 3.0f) && near(ph[1], 1.0f),
          "3x: two thirds, then the real frame (%d)", n);
    CHECK(n == 2 && at[1] - t > 2 * I / 3 - MS / 10 &&
          at[1] - t < 2 * I / 3 + MS / 10, "3x: real frame 2/3 frame late");
}

static void test_refresh_cap(void)
{
    struct nb_fg f;
    uint64_t t = 1000 * MS, I = 8333333ull;   /* 120 fps */
    float first = 0;

    /* 120 fps on 240 Hz: 2x fits, 3x does not. */
    nb_fg_init(&f, 3);
    nb_fg_set_refresh(&f, 4166667ull);
    nb_fg_arrival(&f, t, false);
    for (int i = 0; i < 10; i++) {
        t += I;
        first = nb_fg_arrival(&f, t, true);
    }
    CHECK(f.eff == 2 && near(first, 0.5f), "3x capped to 2x (%d)", f.eff);
    /* 200 fps on 240 Hz: nothing to add. */
    nb_fg_init(&f, 2);
    nb_fg_set_refresh(&f, 4166667ull);
    nb_fg_arrival(&f, t, false);
    for (int i = 0; i < 10; i++) {
        t += 5 * MS;
        first = nb_fg_arrival(&f, t, true);
    }
    CHECK(f.eff == 1 && near(first, 1.0f) && nb_fg_due(&f) == 0,
          "guest near the refresh rate: generation steps aside");
    /* Unknown refresh: the multiplier as asked. */
    nb_fg_init(&f, 3);
    nb_fg_arrival(&f, t, false);
    for (int i = 0; i < 10; i++) {
        t += 5 * MS;
        nb_fg_arrival(&f, t, true);
    }
    CHECK(f.eff == 3, "no refresh known: 3x as asked");
}

static void test_pause_and_preempt(void)
{
    struct nb_fg f;
    uint64_t t = 1000 * MS, I = 16666667ull;
    float first;
    int step = 0;

    nb_fg_init(&f, 2);
    nb_fg_arrival(&f, t, false);
    for (int i = 0; i < 5; i++) {
        t += I;
        nb_fg_arrival(&f, t, true);
    }
    CHECK(nb_fg_due(&f) != 0, "a cycle is pending");
    /* The next frame comes early: the pending step is abandoned (as each
     * cycle above was: nothing ran their timer). */
    {
        uint64_t before = f.dropped;

        t += I / 4;
        nb_fg_arrival(&f, t, true);
        CHECK(f.dropped == before + 1, "pending step dropped (%llu)",
              (unsigned long long)f.dropped);
    }
    /* A pause (a loading screen): no sliding from the stale frame. */
    t += 500 * MS;
    first = nb_fg_arrival(&f, t, true);
    CHECK(near(first, 1.0f) && nb_fg_due(&f) == 0, "after a pause: as it is");
    CHECK(f.interval < 20 * MS, "the pause does not enter the average");
    /* Not shown when due (no free buffer): asked again, then skipped
     * ahead when a later step came due meanwhile (3x). */
    nb_fg_init(&f, 3);
    t = 2000 * MS;
    nb_fg_arrival(&f, t, false);
    for (int i = 0; i < 5; i++) {
        t += 30 * MS;
        nb_fg_arrival(&f, t, true);
    }
    CHECK(near(nb_fg_peek(&f, t + 11 * MS, &step), 2.0f / 3.0f) && step == 1,
          "step 1 due after a third");
    CHECK(near(nb_fg_peek(&f, t + 21 * MS, &step), 1.0f) && step == 2,
          "late: straight to the real frame");
    nb_fg_shown(&f, step);
    CHECK(f.skipped == 1 && nb_fg_due(&f) == 0, "one skipped, cycle over");
    CHECK(nb_fg_peek(&f, t + 5 * MS, &step) == 0.0f, "nothing due");
    /* Abandoned: nothing due, the interval kept, the next frame pairs. */
    nb_fg_arrival(&f, t + 30 * MS, true);
    nb_fg_abandon(&f);
    CHECK(nb_fg_due(&f) == 0 && f.interval > 25 * MS, "abandoned cycle");
    CHECK(nb_fg_arrival(&f, t + 60 * MS, true) < 1.0f, "next frame pairs");
    t += 60 * MS;
    /* Reset forgets the pair. */
    nb_fg_reset(&f);
    t += 30 * MS;
    CHECK(near(nb_fg_arrival(&f, t, true), 1.0f), "after reset: first frame");
}

static void test_off(void)
{
    struct nb_fg f;
    uint64_t t = 1000 * MS;

    nb_fg_init(&f, 1);
    for (int i = 0; i < 5; i++) {
        t += 16 * MS;
        CHECK(near(nb_fg_arrival(&f, t, true), 1.0f), "off: always the frame");
    }
    CHECK(nb_fg_due(&f) == 0, "off: never a timer");
    nb_fg_init(&f, 9);
    CHECK(f.mult == NB_FG_MAX_MULT, "multiplier clamped");
}

int main(void)
{
    test_2x_steady();
    test_3x_phases();
    test_refresh_cap();
    test_pause_and_preempt();
    test_off();
    printf("test_fg: %d checks, %d failed\n", checks, fails);
    return fails ? 1 : 0;
}
