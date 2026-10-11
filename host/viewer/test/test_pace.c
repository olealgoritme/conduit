/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * test_pace.c -- the commit gate (nb_pace.h) under a frame flood.
 *
 * Simulates the viewer's loop: a guest flipping at 7000 fps, a compositor
 * whose frame callbacks fire at 240 Hz (vsync) or right after each commit's
 * render (tearing), a socket that backs up, and a window whose callbacks stop
 * (hidden).  Checks the request rate stays bounded, the newest frame is the
 * one committed, and nothing is held back forever.
 */
#include <stdio.h>
#include <stdint.h>

#include "nb_pace.h"

static int checks, fails;
#define CHECK(c, ...) do { checks++; if (!(c)) { fails++; \
        printf("FAIL %s:%d: ", __FILE__, __LINE__); printf(__VA_ARGS__); \
        printf("\n"); } } while (0)

#define MS 1000000ull

struct sim {
    struct nb_pace p;
    bool     staged;            /* a frame waits for the gate             */
    uint32_t staged_seq;
    uint32_t committed_seq;     /* newest frame committed                 */
    unsigned commits, released;
    bool     armed_pending;     /* a frame callback is outstanding        */
};

/* What wl_commit does with the gate: commit, or keep the frame staged. */
static void try_commit(struct sim *s, uint64_t now)
{
    bool arm;

    if (!s->staged || !nb_pace_may_commit(&s->p, now)) {
        if (s->staged) {
            s->p.deferred++;
        }
        return;
    }
    arm = !s->armed_pending;
    s->armed_pending = true;
    nb_pace_committed(&s->p, now, arm);
    s->committed_seq = s->staged_seq;
    s->staged = false;
    s->commits++;
}

/* A guest frame: a newer ATTACH supersedes (and releases) a staged one. */
static void frame_in(struct sim *s, uint32_t seq, uint64_t now)
{
    if (s->staged) {
        s->released++;
    }
    s->staged = true;
    s->staged_seq = seq;
    try_commit(s, now);
}

static void callback(struct sim *s, uint64_t now)
{
    if (!s->armed_pending) {
        return;
    }
    s->armed_pending = false;
    nb_pace_frame_done(&s->p);
    try_commit(s, now);         /* the staged newest goes out now */
}

/* 7000 fps for one second against 240 Hz callbacks (vsync). */
static void test_vsync_flood(void)
{
    struct sim s = { 0 };
    uint64_t t, next_cb = 4166666, period = 1000000000ull / 7000;
    uint32_t seq = 0;

    for (t = 0; t < 1000 * MS; t += period) {
        while (next_cb <= t) {
            callback(&s, next_cb);
            next_cb += 4166666;
        }
        frame_in(&s, ++seq, t);
    }
    /* <= NB_PACE_AHEAD per callback, plus the one at each callback */
    CHECK(s.commits <= 240 * (NB_PACE_AHEAD + 1) + 4,
          "vsync flood: %u commits in 1 s (bounded)", s.commits);
    CHECK(s.commits >= 240, "vsync flood: at least one per refresh (%u)",
          s.commits);
    CHECK(s.commits + s.released + (s.staged ? 1 : 0) == seq,
          "every frame committed, released or staged (%u+%u vs %u)",
          s.commits, s.released, seq);
    CHECK(s.p.deferred > 0, "frames were held back");
}

/* Tearing: the callback fires ~0.5 ms after each armed commit. */
static void test_tearing_flood(void)
{
    struct sim s = { 0 };
    uint64_t t, period = 1000000000ull / 7000, cb_at = 0;
    uint32_t seq = 0;

    for (t = 0; t < 1000 * MS; t += period) {
        if (s.armed_pending && cb_at && t >= cb_at) {
            callback(&s, cb_at);
            cb_at = 0;
        }
        frame_in(&s, ++seq, t);
        if (s.armed_pending && !cb_at) {
            cb_at = t + MS / 2;
        }
    }
    CHECK(s.commits <= 2000 * NB_PACE_AHEAD, "tearing flood: %u commits, not 7000",
          s.commits);
    CHECK(s.commits > 1000, "tearing: ~one per render (%u)", s.commits);
}

/* The newest frame is what goes out when the gate opens. */
static void test_newest_wins(void)
{
    struct sim s = { 0 };

    frame_in(&s, 1, 0);         /* armed, ahead 1 */
    frame_in(&s, 2, 1);         /* ahead 2 */
    frame_in(&s, 3, 2);         /* held */
    frame_in(&s, 4, 3);         /* replaces 3 */
    frame_in(&s, 5, 4);         /* replaces 4 */
    CHECK(s.committed_seq == 2 && s.staged && s.staged_seq == 5,
          "held: committed %u, staged %u", s.committed_seq, s.staged_seq);
    CHECK(s.released == 2, "superseded frames released (%u)", s.released);
    callback(&s, 5 * MS);
    CHECK(s.committed_seq == 5 && !s.staged, "callback commits the newest");
}

/* A full socket holds everything until a flush succeeds. */
static void test_socket_full(void)
{
    struct sim s = { 0 };

    nb_pace_flushed(&s.p, true);
    frame_in(&s, 1, 0);
    frame_in(&s, 2, 1);
    CHECK(s.commits == 0 && s.staged, "nothing written while EAGAIN");
    CHECK(nb_pace_due_ns(&s.p, 1) == 0, "no timer: POLLOUT reopens it");
    nb_pace_flushed(&s.p, false);
    try_commit(&s, 2);
    CHECK(s.commits == 1 && s.committed_seq == 2, "after the flush: newest");
}

/* Callbacks stop (hidden window): 20 commits a second, not zero, not 7000. */
static void test_stuck_callback(void)
{
    struct sim s = { 0 };
    uint64_t t, period = 1000000000ull / 7000;
    uint32_t seq = 0;

    for (t = 0; t < 1000 * MS; t += period) {
        frame_in(&s, ++seq, t);
    }
    CHECK(s.commits >= 18 && s.commits <= 24,
          "stuck callback: %u commits in 1 s", s.commits);
    CHECK(nb_pace_due_ns(&s.p, t) > 0 && nb_pace_due_ns(&s.p, t) <=
          NB_PACE_STUCK_NS, "a deadline while held by an overdue callback");
}

/*
 * Cursor nudges: 1000 Hz pointer motion over an idle fullscreen picture (no
 * guest frames), callbacks ~0.5 ms after each commit (tearing).  The cursor
 * is repainted at the refresh-bounded rate, never held for good, never more
 * than the gate allows.
 */
static void test_cursor_nudge(void)
{
    struct nb_pace p = { 0 };
    uint64_t t, cb_at = 0, min_ns = 500000000000ull / 240000; /* 240 Hz */
    unsigned nudges = 0, pending = 0;
    bool armed = false, want_pending = false;

    CHECK(nb_nudge_wanted(true, true, true, true), "nudge: fullscreen immediate");
    CHECK(!nb_nudge_wanted(false, true, true, true), "no nudge with vsync");
    CHECK(!nb_nudge_wanted(true, false, true, true), "no nudge windowed");
    CHECK(!nb_nudge_wanted(true, true, false, true), "no nudge under grab");
    CHECK(!nb_nudge_wanted(true, true, true, false), "no nudge without a frame");

    for (t = 1; t < 1000 * MS; t += MS / 4) {   /* 4 kHz loop */
        if (armed && cb_at && t >= cb_at) {
            armed = false;
            cb_at = 0;
            nb_pace_frame_done(&p);
        }
        if (t % MS < MS / 4) {
            want_pending = true;                /* a motion event, 1 kHz */
        }
        if (want_pending) {
            if (nb_nudge_may(&p, t, min_ns)) {
                bool arm = !armed;

                armed = true;
                nb_pace_committed(&p, t, arm);
                if (arm) {
                    cb_at = t + MS / 2;
                }
                nudges++;
                want_pending = false;
            } else {
                pending++;
                CHECK(nb_nudge_due_ns(&p, t, min_ns) > 0 || armed,
                      "a held nudge has a way out");
            }
        }
    }
    CHECK(nudges >= 400 && nudges <= 480,
          "cursor repainted %u times/s at 240 Hz (~2x refresh)", nudges);
    CHECK(pending > 0, "faster motion is coalesced");

    /* A full socket holds nudges too, until the flush succeeds. */
    nb_pace_flushed(&p, true);
    CHECK(!nb_nudge_may(&p, t + 10 * MS, min_ns), "no nudge while EAGAIN");
    CHECK(nb_nudge_due_ns(&p, t + 10 * MS, min_ns) == 0, "POLLOUT reopens it");
    nb_pace_flushed(&p, false);
}

int main(void)
{
    test_cursor_nudge();
    test_vsync_flood();
    test_tearing_flood();
    test_newest_wins();
    test_socket_full();
    test_stuck_callback();
    printf("test_pace: %d checks, %d failed\n", checks, fails);
    return fails ? 1 : 0;
}
