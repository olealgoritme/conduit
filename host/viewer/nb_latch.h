/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * nb_latch.h -- "latest frame per refresh, late latch" presentation (--sync=latch).
 *
 * The guest renders at its own, uneven rate (a game: 300+ fps with a longer
 * frame every simulation tick); the host output refreshes at a fixed rate.
 * Immediate presentation shows each frame when it arrives, so motion follows
 * the guest's jitter plus the compositor's; vsync presentation commits each
 * frame when the compositor's frame callback allows, so how long a frame waits
 * depends on where in the refresh it arrived and on what was queued ahead.
 *
 * This mode commits at most ONE frame per output refresh, at a fixed point
 * before the vblank it is aimed at: the newest complete guest frame at that
 * moment.  Nothing queues: a newer frame replaces a staged one.  The point is
 * as late as the compositor still takes it for that vblank, so the time from
 * commit to scanout is constant and short, and so is the age of what is shown.
 *
 *   wake(V)     = V - lead - budget - redzone      start of the latch window
 *   deadline(V) = V - lead - budget                 last commit for V
 *
 *   V        a predicted vblank: the last presented timestamp the compositor
 *            reported (wp_presentation, VSYNC kind) stepped by the refresh
 *            period it reported.
 *   lead     how long before V the compositor needs the commit to show it at
 *            V, plus a margin of an eighth of a refresh.  Learned from
 *            feedback: a commit aimed at V that is presented later raises the
 *            floor to the lead that failed; commits on time let the floor sink
 *            by 1/65536 each (at 240 Hz: about 10% a minute), so the
 *            threshold is re-probed rarely -- a probe that fails costs one
 *            repeated refresh.  A compositor that renders right after the
 *            previous vblank (Hyprland by default) ends near one refresh; one
 *            that latches late ends near its own render time.
 *   budget   the viewer's own cost from wake to the commit leaving the socket
 *            (GPU scaling pass + requests): a rolling maximum that decays 2%
 *            per sample.
 *   redzone  scheduling slack (timer wakeup and thread scheduling).
 *
 * At wake, a frame that arrived since the last commit is committed; with none
 * yet, the first one to arrive before the deadline is committed on arrival
 * (the late latch); with none by the deadline, the refresh repeats the
 * previous frame and the window moves to the next vblank.
 *
 * Until the first presentation timestamp arrives there is no vblank clock:
 * frames are committed as they come (vsync behaviour).
 *
 * The model is the vblank timer of Valve's gamescope (wake at next vblank
 * minus a rolling-maximum draw time plus a red zone, take the newest frame);
 * this is an independent implementation for a nested Wayland client, where the
 * vblank is learned from wp_presentation and the compositor's own latch point
 * has to be learned too.
 *
 * Pure logic, no Wayland: test/test_latch.c drives it.
 */
#ifndef NB_LATCH_H
#define NB_LATCH_H

#include <stdbool.h>
#include <stdint.h>

#define NB_LATCH_REDZONE_NS   (500ull * 1000)    /* timer + scheduling slack */
#define NB_LATCH_BUDGET_MIN   (300ull * 1000)    /* own cost floor           */
#define NB_LATCH_BUDGET_START (1000ull * 1000)
#define NB_LATCH_LEAD_MIN     (200ull * 1000)
#define NB_LATCH_LEAD_START   (1000ull * 1000)

struct nb_latch {
    uint64_t period;        /* refresh period, ns (0: no vblank clock yet)    */
    uint64_t anchor;        /* a presented vblank timestamp, ns               */
    uint64_t lead;          /* compositor latch lead incl. margin, ns         */
    uint64_t floor;         /* highest lead that was seen to fail, decaying  */
    uint64_t budget;        /* own wake -> flushed cost, ns                   */
    uint64_t redzone;
    uint64_t target;        /* vblank the current window aims at (0: none)    */
    uint64_t served;        /* last vblank a frame was committed for          */
    /* counters, for --stats / the frame log */
    uint64_t on_time, late, repeats, commits;
};

static inline void nb_latch_init(struct nb_latch *l)
{
    *l = (struct nb_latch){
        .lead = NB_LATCH_LEAD_START,
        .floor = 0,
        .budget = NB_LATCH_BUDGET_START,
        .redzone = NB_LATCH_REDZONE_NS,
    };
}

static inline bool nb_latch_clocked(const struct nb_latch *l)
{
    return l->period != 0 && l->anchor != 0;
}

/* The first vblank after `t` on the anchor's grid (strictly later). */
static inline uint64_t nb_latch_vblank_after(const struct nb_latch *l,
                                             uint64_t t)
{
    uint64_t k;

    if (t < l->anchor) {
        return l->anchor;
    }
    k = (t - l->anchor) / l->period + 1;
    return l->anchor + k * l->period;
}

static inline uint64_t nb_latch_offset(const struct nb_latch *l)
{
    return l->lead + l->budget;
}

/*
 * The vblank the next commit should aim at, seen from `now`: the first one
 * whose deadline has not passed and that was not served already.  Sets
 * l->target.  0 when there is no vblank clock.
 */
static inline uint64_t nb_latch_aim(struct nb_latch *l, uint64_t now)
{
    uint64_t v;

    if (!nb_latch_clocked(l)) {
        return l->target = 0;
    }
    v = nb_latch_vblank_after(l, now + nb_latch_offset(l));
    while (v <= l->served + l->period / 2) {
        v += l->period;
    }
    return l->target = v;
}

/* Start of the latch window for the current target. */
static inline uint64_t nb_latch_wake(const struct nb_latch *l)
{
    uint64_t off = nb_latch_offset(l) + l->redzone;

    return l->target > off ? l->target - off : 0;
}

/*
 * May a staged frame be committed now?  True without a vblank clock (commit
 * as frames come).  Re-aims when the window passed without a commit (that
 * refresh repeats the previous frame).
 */
static inline bool nb_latch_may_commit(struct nb_latch *l, uint64_t now)
{
    if (!nb_latch_clocked(l)) {
        return true;
    }
    if (!l->target || l->target <= l->served ||
        now + nb_latch_offset(l) > l->target) {
        if (l->target && l->target > l->served) {
            l->repeats++;               /* its window passed without a frame */
        }
        nb_latch_aim(l, now);
    }
    return now >= nb_latch_wake(l);
}

/*
 * ns from `now` until the window opens (>= 1), or 0 when it is open already or
 * there is no clock.  The caller arms a timer for it while a frame is staged.
 */
static inline uint64_t nb_latch_due_ns(struct nb_latch *l, uint64_t now)
{
    uint64_t wk;

    if (!nb_latch_clocked(l)) {
        return 0;
    }
    if (!l->target || l->target <= l->served ||
        now + nb_latch_offset(l) > l->target) {
        nb_latch_aim(l, now);
    }
    wk = nb_latch_wake(l);
    return wk > now ? wk - now : 0;
}

/*
 * A frame was committed for the current target.  `cost_ns`: from the start of
 * the commit work (window open or frame arrival, whichever was later) to the
 * flush.  Returns the vblank it aims at (for its presentation feedback).
 */
static inline uint64_t nb_latch_committed(struct nb_latch *l, uint64_t now,
                                          uint64_t cost_ns)
{
    uint64_t aimed = l->target;
    uint64_t decayed = l->budget - l->budget / 50;

    (void)now;
    l->commits++;
    l->budget = cost_ns > decayed ? cost_ns : decayed;
    if (l->budget < NB_LATCH_BUDGET_MIN) {
        l->budget = NB_LATCH_BUDGET_MIN;
    }
    if (l->period && l->budget > l->period) {
        l->budget = l->period;          /* a pathological sample, not a plan */
    }
    if (aimed) {
        l->served = aimed;
    }
    return aimed;
}

/*
 * Presentation feedback for a commit aimed at `aimed` (0: committed without a
 * clock).  `t` is the presented timestamp, `refresh` the period the compositor
 * reported (0 unknown), `vsync` whether the timestamp is a vblank.
 */
static inline void nb_latch_presented(struct nb_latch *l, uint64_t t,
                                      uint64_t refresh, bool vsync,
                                      uint64_t aimed)
{
    if (refresh) {
        l->period = refresh;
    }
    if (!t || !l->period) {
        return;
    }
    if (vsync && t > l->anchor) {
        l->anchor = t;
    }
    if (!aimed) {
        return;
    }
    if (t > aimed + l->period / 2) {
        l->late++;
        if (l->lead > l->floor) {
            l->floor = l->lead;         /* this lead was not enough */
        }
    } else {
        l->on_time++;
        l->floor -= l->floor / 65536;
    }
    l->lead = l->floor + l->period / 8;
    if (l->lead < NB_LATCH_LEAD_MIN) {
        l->lead = NB_LATCH_LEAD_MIN;
    }
    if (l->lead > 2 * l->period) {
        l->lead = 2 * l->period;
    }
}

#endif /* NB_LATCH_H */
