/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * test_latch.c -- "latest frame per refresh, late latch" (nb_latch.h).
 *
 * Simulates the viewer's loop against a compositor on a 240 Hz output that
 * shows a commit at vblank V when it arrived at least `clead` before V
 * (Hyprland renders right after the previous vblank: clead ~ one refresh; a
 * late-latching compositor: ~1 ms), and a guest that renders like CS2 does in
 * the VM (four 2.3 ms frames, then a 6.7 ms simulation-tick frame).  Checks:
 * one commit per refresh at most, the lead converges past the compositor's
 * latch point and then stays there (few late frames), the frame shown is the
 * newest one that existed at the deadline, the commit-to-scanout time is
 * constant, and without a vblank clock frames are committed as they come.
 */
#include <stdio.h>
#include <stdint.h>
#include <stdlib.h>
#include <math.h>

#include "nb_latch.h"

static int checks, fails;
#define CHECK(c, ...) do { checks++; if (!(c)) { fails++; \
        printf("FAIL %s:%d: ", __FILE__, __LINE__); printf(__VA_ARGS__); \
        printf("\n"); } } while (0)

#define US 1000ull
#define MS 1000000ull
#define PERIOD 4166667ull

struct pending { uint64_t commit, aimed; uint32_t seq; uint64_t arrived; };

struct sim {
    struct nb_latch l;
    uint64_t clead;             /* compositor: commit at least this before V */
    uint64_t cost;              /* viewer's commit cost                      */
    /* guest */
    uint64_t next_frame;
    unsigned frame_i;
    uint32_t seq;
    bool     staged;
    uint32_t staged_seq;
    uint64_t staged_at;
    /* compositor */
    struct pending q[8];
    int      nq;
    /* results */
    unsigned commits, shown, late_after_warmup, refreshes;
    uint64_t lat_sum, lat_sq, lat_n;      /* commit -> scanout             */
    uint64_t age_sum, age_n;              /* newest frame arrival -> scanout */
    unsigned stale;                       /* shown frame older than newest at deadline */
    uint64_t last_commit_v;
    unsigned two_in_one;
};

static uint64_t frame_gap(unsigned i)
{
    return (i % 5 == 4) ? 6700 * US : 2300 * US;
}

/* The compositor at vblank v: show the newest commit that made it. */
static void vblank(struct sim *s, uint64_t v, bool warm)
{
    int i, best = -1;

    s->refreshes++;
    for (i = 0; i < s->nq; i++) {
        if (s->q[i].commit + s->clead <= v) {
            best = i;
        }
    }
    if (best < 0) {
        return;
    }
    {
        struct pending p = s->q[best];
        uint64_t lat = v - p.commit;

        nb_latch_presented(&s->l, v, PERIOD, true, p.aimed);
        if (warm) {
            s->shown++;
            s->lat_sum += lat; s->lat_sq += lat * lat; s->lat_n++;
            s->age_sum += v - p.arrived; s->age_n++;
            if (p.aimed && v > p.aimed + PERIOD / 2) {
                s->late_after_warmup++;
            }
        }
    }
    /* drop what was consumed (and anything older) */
    for (i = best + 1; i < s->nq; i++) {
        s->q[i - best - 1] = s->q[i];
    }
    s->nq -= best + 1;
}

static void try_commit(struct sim *s, uint64_t now)
{
    uint64_t aimed;

    if (!s->staged || !nb_latch_may_commit(&s->l, now)) {
        return;
    }
    aimed = nb_latch_committed(&s->l, now, s->cost);
    if (aimed && aimed == s->last_commit_v) {
        s->two_in_one++;
    }
    s->last_commit_v = aimed;
    if (s->nq < 8) {
        s->q[s->nq++] = (struct pending){ now + s->cost, aimed, s->staged_seq,
                                          s->staged_at };
    }
    s->staged = false;
    s->commits++;
}

static void run(struct sim *s, uint64_t clead, uint64_t seconds, bool clock)
{
    uint64_t t, v = 10 * MS + PERIOD, end = seconds * 1000 * MS;
    uint64_t warm = end / 4;

    *s = (struct sim){ .clead = clead, .cost = 700 * US };
    nb_latch_init(&s->l);
    s->next_frame = 3 * MS;
    if (clock) {
        /* first feedback: a presentation of an earlier (unaimed) commit */
        nb_latch_presented(&s->l, 10 * MS, PERIOD, true, 0);
    }
    for (t = 0; t < end; t += 50 * US) {
        if (t >= s->next_frame) {
            s->seq++;
            s->staged = true;           /* newest replaces a staged one */
            s->staged_seq = s->seq;
            s->staged_at = t;
            s->next_frame += frame_gap(s->frame_i++);
            try_commit(s, t);
        }
        /* the viewer's timer: wakes at the window start */
        if (s->staged && nb_latch_due_ns(&s->l, t) == 0) {
            try_commit(s, t);
        }
        if (t >= v) {
            vblank(s, v, t > warm);
            v += PERIOD;
        }
    }
}

int main(void)
{
    struct sim s;
    double mean, sd;

    /* Hyprland-like: the commit must be in before the previous vblank's render */
    run(&s, PERIOD - 300 * US, 20, true);
    mean = (double)s.lat_sum / s.lat_n;
    sd = 0;
    {
        double m2 = (double)s.lat_sq / s.lat_n - mean * mean;
        sd = m2 > 0 ? sqrt(m2) : 0;
    }
    printf("hyprland-like: commits %u, refreshes %u, shown %u, late after warmup %u, "
           "lead %.2f ms, commit->scanout %.2f ms sd %.3f, age %.2f ms\n",
           s.commits, s.refreshes, s.shown, s.late_after_warmup,
           s.l.lead / 1e6, mean / 1e6, sd / 1e6, (double)s.age_sum / s.age_n / 1e6);
    CHECK(s.two_in_one == 0, "two commits aimed at one vblank: %u", s.two_in_one);
    CHECK(s.commits <= s.refreshes + 2, "more commits (%u) than refreshes (%u)",
          s.commits, s.refreshes);
    CHECK(s.l.lead >= PERIOD - 300 * US, "lead %llu below the compositor's latch",
          (unsigned long long)s.l.lead);
    CHECK(s.late_after_warmup <= 3, "late frames after warmup: %u", s.late_after_warmup);
    CHECK(sd < 0.2 * MS, "commit->scanout not constant: sd %.0f ns", sd);
    CHECK(mean < PERIOD + 2 * MS, "commit->scanout %.0f ns too long", mean);
    /* Most refreshes get a new frame: with 6.7 ms tick frames some must repeat. */
    CHECK(s.shown > s.refreshes * 3 / 4 * 3 / 4, "too few new frames shown: %u of %u",
          s.shown, s.refreshes * 3 / 4);

    /* late-latching compositor: 1 ms */
    run(&s, 1 * MS, 20, true);
    mean = (double)s.lat_sum / s.lat_n;
    {
        double m2 = (double)s.lat_sq / s.lat_n - mean * mean;
        sd = m2 > 0 ? sqrt(m2) : 0;
    }
    printf("late-latch compositor: commits %u, refreshes %u, shown %u, late after warmup %u, "
           "lead %.2f ms, commit->scanout %.2f ms sd %.3f, age %.2f ms\n",
           s.commits, s.refreshes, s.shown, s.late_after_warmup,
           s.l.lead / 1e6, mean / 1e6, sd / 1e6, (double)s.age_sum / s.age_n / 1e6);
    CHECK(s.l.lead < 2 * MS, "lead %llu did not stay near 1 ms", (unsigned long long)s.l.lead);
    CHECK(s.late_after_warmup <= 3, "late frames after warmup: %u", s.late_after_warmup);
    CHECK(mean < 2.5 * MS, "commit->scanout %.0f ns, want <= ~2 ms", mean);
    CHECK(sd < 0.2 * MS, "commit->scanout not constant: sd %.0f ns", sd);
    CHECK(s.two_in_one == 0, "two commits aimed at one vblank: %u", s.two_in_one);

    /* no vblank clock yet: frames go out as they come */
    {
        struct nb_latch l;

        nb_latch_init(&l);
        CHECK(nb_latch_may_commit(&l, 5 * MS), "unclocked latch held a frame");
        CHECK(nb_latch_due_ns(&l, 5 * MS) == 0, "unclocked latch armed a timer");
        CHECK(nb_latch_committed(&l, 5 * MS, 700 * US) == 0, "unclocked commit aimed");
    }

    /* budget: rolling max with decay, bounded */
    {
        struct nb_latch l;
        int i;

        nb_latch_init(&l);
        nb_latch_presented(&l, 10 * MS, PERIOD, true, 0);
        nb_latch_aim(&l, 11 * MS);
        nb_latch_committed(&l, 11 * MS, 3 * MS);
        CHECK(l.budget == 3 * MS, "budget did not jump to the max: %llu",
              (unsigned long long)l.budget);
        for (i = 0; i < 400; i++) {
            nb_latch_committed(&l, 11 * MS, 100 * US);
        }
        CHECK(l.budget == NB_LATCH_BUDGET_MIN, "budget did not decay to the floor: %llu",
              (unsigned long long)l.budget);
        nb_latch_committed(&l, 11 * MS, 50 * MS);
        CHECK(l.budget == PERIOD, "budget not capped at a period: %llu",
              (unsigned long long)l.budget);
    }

    /* the window: aims past `now + offset`, never at a served vblank */
    {
        struct nb_latch l;
        uint64_t v1, v2;

        nb_latch_init(&l);
        nb_latch_presented(&l, 100 * MS, PERIOD, true, 0);
        v1 = nb_latch_aim(&l, 100 * MS);
        CHECK(v1 > 100 * MS + nb_latch_offset(&l), "aimed inside the offset");
        CHECK((v1 - 100 * MS) % PERIOD == 0, "aim off the vblank grid");
        nb_latch_committed(&l, nb_latch_wake(&l), 700 * US);
        v2 = nb_latch_aim(&l, nb_latch_wake(&l));
        CHECK(v2 == v1 + PERIOD, "re-aimed at a served vblank (%llu vs %llu)",
              (unsigned long long)v2, (unsigned long long)v1);
    }

    printf("%d checks, %d failed\n", checks, fails);
    return fails ? 1 : 0;
}
