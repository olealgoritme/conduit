/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * nb_fg.h -- when the viewer shows generated frames (--framegen=2x|3x,
 * docs/VIEWER-ENHANCE.md).
 *
 * Generation interpolates between the last two guest frames, so it can only
 * show what lies between frames it already has: frame N-1 to N is played
 * back while frame N+1 is being rendered.  With a multiplier m and a guest
 * frame interval I, each arriving frame N starts a cycle of m pictures, one
 * every I/m:
 *
 *   arrival of N      interp(N-1, N, 1/m)     shown at once
 *   + I/m             interp(N-1, N, 2/m)     (3x only)
 *   + (m-1) I/m       N itself
 *
 * so N reaches the screen (m-1)/m of a frame after it arrived: half a guest
 * frame at 2x, two thirds at 3x, plus the GPU passes.  The UI rounds that to
 * "about one guest frame".
 *
 * I is measured: a running average of the arrival deltas.  The guest is
 * never waited for or throttled -- its buffer goes back the moment the pass
 * has read it, so generation adds nothing to the guest's present path.
 *
 * The multiplier comes down by itself where generating would not show:
 *   - no pair yet (first frame, a mode change, after a reset)  -> 1
 *   - a pause: the delta is over NB_FG_GAP_NS or 4x the average (a menu, a
 *     loading screen, an idle desktop) -> 1, so the next frame is not slid
 *     towards from a stale one
 *   - the guest already runs near the output's refresh rate: a generated
 *     frame must be at least about one refresh after the last one, so
 *     m <= I / refresh (rounded), e.g. 120 fps on 240 Hz gives 2x at most
 *
 * A cycle not finished when the next frame arrives is abandoned (counted in
 * `dropped`): the newest pair always wins.
 *
 * Pure logic, no GPU or display: test/test_fg.c drives it.
 */
#ifndef NB_FG_H
#define NB_FG_H

#include <stdbool.h>
#include <stdint.h>

#define NB_FG_MAX_MULT  3
#define NB_FG_GAP_NS    (100ull * 1000 * 1000)  /* longer: the guest paused */
#define NB_FG_MIN_NS    (1ull * 1000 * 1000)    /* shorter: not a frame rate */

struct nb_fg {
    int      mult;              /* wanted: 1 (off), 2 or 3                  */
    uint64_t last_ns;           /* last guest frame's arrival, 0 none       */
    uint64_t interval;          /* average guest frame interval, ns, 0 unknown */
    uint64_t refresh;           /* output refresh period, ns, 0 unknown     */
    int      eff;               /* multiplier of the current cycle          */
    int      step;              /* next step of the cycle, 0 = none pending */
    uint64_t base_ns;           /* when the current cycle began             */
    /* counters, for --stats */
    uint64_t generated, dropped, skipped, cycles;
};

static inline void nb_fg_init(struct nb_fg *f, int mult)
{
    *f = (struct nb_fg){ 0 };
    f->mult = mult < 1 ? 1 : mult > NB_FG_MAX_MULT ? NB_FG_MAX_MULT : mult;
}

/* Forget the pair and the cycle (mode change, frame generation turned off
 * and on); the measured interval stays. */
static inline void nb_fg_reset(struct nb_fg *f)
{
    if (f->step) {
        f->dropped += (uint64_t)(f->eff - f->step);
    }
    f->step = 0;
    f->eff = 1;
    f->last_ns = 0;
}

/* Give up the rest of the current cycle (a newer guest frame is about to
 * start its own); the pair and the interval stay. */
static inline void nb_fg_abandon(struct nb_fg *f)
{
    if (f->step) {
        f->dropped += (uint64_t)(f->eff - f->step);
    }
    f->step = 0;
}

static inline void nb_fg_set_refresh(struct nb_fg *f, uint64_t period_ns)
{
    f->refresh = period_ns;
}

/* The multiplier for a guest interval of `interval` ns. */
static inline int nb_fg_eff(const struct nb_fg *f, uint64_t interval)
{
    int m = f->mult;

    if (!interval || m <= 1) {
        return 1;
    }
    if (f->refresh) {
        /* Each generated picture at least ~one refresh apart: round I/R
         * half up, so 120 fps on 240 Hz still makes 2x. */
        uint64_t fit = (interval + f->refresh / 2) / f->refresh;

        if (fit < (uint64_t)m) {
            m = fit < 1 ? 1 : (int)fit;
        }
    }
    return m;
}

/*
 * Guest frame arrived at `now`; `pair` says whether the GPU has the previous
 * frame to interpolate from (nb_gs_fg_push returned 1).  Returns the phase
 * to show now: 1.0 is the new frame itself, below 1 a generated picture
 * that far from the previous frame towards the new one.
 */
static inline float nb_fg_arrival(struct nb_fg *f, uint64_t now, bool pair)
{
    uint64_t d = f->last_ns && now > f->last_ns ? now - f->last_ns : 0;
    bool gap = !f->last_ns || d > NB_FG_GAP_NS ||
               (f->interval && d > 4 * f->interval);

    if (f->step) {
        f->dropped += (uint64_t)(f->eff - f->step);
        f->step = 0;
    }
    f->last_ns = now;
    if (d >= NB_FG_MIN_NS && d <= NB_FG_GAP_NS) {
        /* An eighth of each new delta: steady within a few frames, and one
         * long frame (a hitch) moves it little. */
        f->interval = f->interval ? (f->interval * 7 + d) / 8 : d;
    }
    f->eff = gap || !pair ? 1 : nb_fg_eff(f, f->interval);
    if (f->eff <= 1) {
        return 1.0f;
    }
    f->cycles++;
    f->generated++;
    f->base_ns = now;
    f->step = 1;
    return 1.0f / (float)f->eff;
}

/* When the next picture of the cycle is due, 0 = nothing pending. */
static inline uint64_t nb_fg_due(const struct nb_fg *f)
{
    if (!f->step || f->eff <= 1) {
        return 0;
    }
    return f->base_ns + f->interval * (uint64_t)f->step / (uint64_t)f->eff;
}

/*
 * The picture to show at `now`, or 0 when none is due.  A step that came due
 * while an earlier one could not be shown is skipped in favour of the latest
 * due one.  Call nb_fg_shown() when the picture was actually presented; one
 * that could not be (no free buffer) stays due and is asked for again.
 */
static inline float nb_fg_peek(const struct nb_fg *f, uint64_t now, int *step)
{
    int s = f->step;

    if (!s || f->eff <= 1 || now < nb_fg_due(f)) {
        return 0.0f;
    }
    while (s + 1 < f->eff &&
           now >= f->base_ns + f->interval * (uint64_t)(s + 1) /
                                   (uint64_t)f->eff) {
        s++;
    }
    *step = s;
    return (float)(s + 1) / (float)f->eff;
}

static inline void nb_fg_shown(struct nb_fg *f, int step)
{
    if (!f->step || step < f->step) {
        return;
    }
    f->skipped += (uint64_t)(step - f->step);
    if (step + 1 < f->eff) {
        f->generated++;
    }
    f->step = step + 1 < f->eff ? step + 1 : 0;
}

#endif /* NB_FG_H */
