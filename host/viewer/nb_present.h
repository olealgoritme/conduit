/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * nb_present.h -- how a guest frame reaches the display server: the present
 * levels, the order --present-mode=auto walks them in, the layouts the GPU
 * copy draws into, and when a buffer handed to the compositor is free again.
 *
 * THREE LEVELS, cheapest first:
 *
 *   ZERO-COPY  the guest's dma-buf itself becomes the window's buffer (a
 *              wl_buffer, an X pixmap).  No work in the viewer.  Needs the
 *              display server to take the guest's format/modifier AND to show
 *              what it took.
 *   GPU-COPY   the viewer's GPU pass (nb_gpuscale.h) draws the guest frame,
 *              scaled and letterboxed, into a buffer the viewer allocated in a
 *              layout the display server lists for rendering (the dma-buf
 *              feedback's main tranche, or the plain modifier list).  One GPU
 *              pass per frame; the buffer is an ordinary client buffer, the
 *              kind every compositor shows.
 *   READBACK   the same pass, read back into shared memory (wl_shm, X
 *              PutImage).  One pass plus a copy through memory per frame; the
 *              one level that needs nothing from the display server but its
 *              core protocol.
 *
 * --present-mode picks where to start and how far down to go:
 *
 *   auto      zero-copy, then gpu-copy, then readback: a level is left for the
 *             next on a HARD failure (the display refused an import, refused
 *             the copy's buffers, has no dma-buf protocol or no usable
 *             layout).  Nothing softer moves it: a client cannot see the
 *             screen, and the signals it does get (presentation feedback,
 *             buffer releases) look the same for a hidden window as for a
 *             buffer the display failed to draw, so guessing from them would
 *             give up zero-copy -- and its latency -- where it works.  A
 *             display that takes the guest's buffers and draws them wrong is
 *             what `copy` (or the viewer.present setting) is for.
 *   native    zero-copy; a format the display does not take is still copied
 *             frame by frame, but the level never changes.
 *   linear    zero-copy of LINEAR buffers only (the VMM detiles); no copy.
 *   copy      gpu-copy, then readback.
 *   readback  readback only.
 *   shm       no dma-buf is advertised; a dma-buf frame that arrives anyway
 *             is read back, a shared-memory frame is shown as it is.
 *
 * Pure logic, no display server: test/test_present.c drives it.
 */
#ifndef NB_PRESENT_H
#define NB_PRESENT_H

#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "nb_scanout.h"

enum nb_pl { NB_PL_ZC = 0, NB_PL_COPY, NB_PL_READBACK, NB_PL_COUNT };

/*
 * The --present-mode values.  The first four keep the numbers the presentation
 * tiers always had (nvkvm_broker.h NB_TIER_*), so nb_tier holds one of these.
 */
enum {
    NB_PM_AUTO = 0,
    NB_PM_NATIVE,
    NB_PM_LINEAR,
    NB_PM_SHM,
    NB_PM_COPY,
    NB_PM_READBACK,
    NB_PM_COUNT
};

static inline const char *nb_pl_name(enum nb_pl l)
{
    switch (l) {
    case NB_PL_ZC:       return "zero-copy";
    case NB_PL_COPY:     return "gpu-copy";
    case NB_PL_READBACK: return "readback";
    default:             return "none";
    }
}

/* 0 and *mode set, or -1 for an unknown word. */
static inline int nb_pm_parse(const char *s, int *mode)
{
    static const struct { const char *name; int m; } t[] = {
        { "auto", NB_PM_AUTO }, { "native", NB_PM_NATIVE },
        { "zero-copy", NB_PM_NATIVE }, { "linear", NB_PM_LINEAR },
        { "shm", NB_PM_SHM }, { "copy", NB_PM_COPY },
        { "gpu-copy", NB_PM_COPY }, { "readback", NB_PM_READBACK },
    };

    if (!s) {
        return -1;
    }
    for (size_t i = 0; i < sizeof(t) / sizeof(t[0]); i++) {
        if (!strcmp(s, t[i].name)) {
            *mode = t[i].m;
            return 0;
        }
    }
    return -1;
}

static inline const char *nb_pm_name(int mode)
{
    switch (mode) {
    case NB_PM_AUTO:     return "auto";
    case NB_PM_NATIVE:   return "native";
    case NB_PM_LINEAR:   return "linear";
    case NB_PM_SHM:      return "shm";
    case NB_PM_COPY:     return "copy";
    case NB_PM_READBACK: return "readback";
    default:             return "?";
    }
}

/* ── the cascade ─────────────────────────────────────────────────────── */

struct nb_pc {
    int        mode;            /* NB_PM_*                                  */
    enum nb_pl level;           /* where frames go now                      */
    enum nb_pl bottom;          /* how far a failure may take it            */
    bool       changed;         /* level changed since the last take        */
    char       why[200];        /* why it is at this level                  */
};

static inline void nb_pc_init(struct nb_pc *c, int mode)
{
    memset(c, 0, sizeof(*c));
    c->mode = mode;
    switch (mode) {
    case NB_PM_AUTO:
        c->level = NB_PL_ZC;
        c->bottom = NB_PL_READBACK;
        break;
    case NB_PM_COPY:
        c->level = NB_PL_COPY;
        c->bottom = NB_PL_READBACK;
        break;
    case NB_PM_READBACK:
    case NB_PM_SHM:
        c->level = c->bottom = NB_PL_READBACK;
        break;
    default:                    /* native, linear: zero-copy, fixed */
        c->level = c->bottom = NB_PL_ZC;
        break;
    }
    snprintf(c->why, sizeof(c->why), "--present-mode=%s", nb_pm_name(mode));
    c->changed = true;
}

/* Whether a dma-buf frame the display cannot take as it is may still be
 * copied by the GPU pass in this mode (linear: never). */
static inline bool nb_pc_copy_allowed(const struct nb_pc *c)
{
    return c->mode != NB_PM_LINEAR;
}

/* Every dma-buf frame goes through the GPU pass at this level. */
static inline bool nb_pc_forces_copy(const struct nb_pc *c)
{
    return c->level >= NB_PL_COPY;
}

/* The GPU pass reads back into shared memory at this level. */
static inline bool nb_pc_forces_readback(const struct nb_pc *c)
{
    return c->level >= NB_PL_READBACK;
}

/*
 * A hard failure at level `at`: go one level down when the mode allows it.
 * Only a failure of the level in force counts (a late answer about a level
 * already left changes nothing).  Returns true when the level changed; the
 * caller logs nb_pc's transition once (nb_pc_take_change).
 */
static inline bool nb_pc_fail(struct nb_pc *c, enum nb_pl at,
                              const char *fmt, ...)
    __attribute__((format(printf, 3, 4)));
static inline bool nb_pc_fail(struct nb_pc *c, enum nb_pl at,
                              const char *fmt, ...)
{
    va_list ap;

    if (at != c->level || c->level >= c->bottom) {
        return false;
    }
    c->level = (enum nb_pl)(c->level + 1);
    va_start(ap, fmt);
    vsnprintf(c->why, sizeof(c->why), fmt, ap);
    va_end(ap);
    c->changed = true;
    return true;
}

/*
 * The display can take no dma-buf at all (no protocol for it, no usable
 * layout): straight to the bottom the mode allows.  True when that is the
 * read-back level, which needs nothing but the core protocol; false means
 * the mode asked for dma-buf (native, linear) and the caller must fail.
 */
static inline bool nb_pc_floor(struct nb_pc *c, const char *why)
{
    while (nb_pc_fail(c, c->level, "%s", why)) {
    }
    return c->level == NB_PL_READBACK;
}

static inline bool nb_pc_take_change(struct nb_pc *c)
{
    bool ch = c->changed;

    c->changed = false;
    return ch;
}

/* ── the GPU copy's buffer layout ────────────────────────────────────── */

#define NB_PC_FCC_XR24 0x34325258u  /* DRM_FORMAT_XRGB8888 */
#define NB_PC_FCC_AR24 0x34325241u  /* DRM_FORMAT_ARGB8888 */
#define NB_PC_MOD_INVALID 0x00ffffffffffffffull

#define NB_PC_CAND 48
struct nb_pc_cand {
    uint32_t fourcc[NB_PC_CAND];
    uint64_t mod[NB_PC_CAND];
    unsigned n;
};

static inline bool nb_pc_cand_has(const struct nb_pc_cand *c, uint32_t f,
                                  uint64_t m)
{
    for (unsigned i = 0; i < c->n; i++) {
        if (c->fourcc[i] == f && c->mod[i] == m) {
            return true;
        }
    }
    return false;
}

static inline void nb_pc_cand_add(struct nb_pc_cand *c, uint32_t f, uint64_t m)
{
    if (m == NB_PC_MOD_INVALID || nb_pc_cand_has(c, f, m) ||
        c->n >= NB_PC_CAND) {
        return;
    }
    c->fourcc[c->n] = f;
    c->mod[c->n] = m;
    c->n++;
}

/*
 * The (format, modifier) pairs to allocate the GPU copy's buffers in, best
 * first, grouped by format: XRGB8888, then ARGB8888 -- 8-bit, whatever the
 * output's own depth, because the guest's frame is 8-bit and the compositor
 * converts to the output in its own pass.  Within a format:
 *
 *   scan    the scanout tranche (feedback), only when `fullscreen` and only
 *           pairs the render set also lists: a buffer a plane can take, which
 *           is what lets the copy reach the screen without composition;
 *   render  the feedback's other tranches (its main device: what it imports
 *           for rendering), when the compositor sent feedback;
 *   v3      the plain zwp_linux_dmabuf_v1 modifier list, when it did not.
 *
 * Implicit modifiers are left out: GBM's own choice is the caller's fallback.
 */
static inline void nb_pc_targets(const struct nb_so_fmts *scan, bool fullscreen,
                                 const struct nb_so_fmts *render,
                                 bool have_render, const struct nb_so_fmts *v3,
                                 struct nb_pc_cand *out)
{
    static const uint32_t fmts[2] = { NB_PC_FCC_XR24, NB_PC_FCC_AR24 };
    const struct nb_so_fmts *base = have_render ? render : v3;

    memset(out, 0, sizeof(*out));
    for (int k = 0; k < 2; k++) {
        uint32_t f = fmts[k];

        if (fullscreen && scan) {
            for (unsigned i = 0; i < scan->n; i++) {
                if (scan->e[i].fourcc == f &&
                    (!base || nb_so_fmts_has(base, f, scan->e[i].mod))) {
                    nb_pc_cand_add(out, f, scan->e[i].mod);
                }
            }
        }
        for (unsigned i = 0; base && i < base->n; i++) {
            if (base->e[i].fourcc == f) {
                nb_pc_cand_add(out, f, base->e[i].mod);
            }
        }
    }
}

/* ── when a committed buffer is free again ───────────────────────────── */
/*
 * Per buffer the viewer hands the compositor (a guest buffer shown zero-copy,
 * one of the GPU copy's buffers).  Without explicit sync the compositor says
 * so with wl_buffer.release.  With wp_linux_drm_syncobj_surface_v1 that event
 * is undefined: the buffer is free when the RELEASE POINT of its latest
 * commit is signalled.  Each buffer has its own release timeline (the
 * protocol's advice: a compositor may signal points out of order across
 * buffers), so its points only grow and `point` is never reset, not even when
 * the slot is reused for another buffer.
 */
struct nb_rel {
    bool     held;              /* the compositor may still read it         */
    bool     xs;                /* its latest commit was explicitly synced  */
    uint64_t point;             /* release point of that commit             */
};

/* Committed: returns the release point to set when `xs`. */
static inline uint64_t nb_rel_commit(struct nb_rel *r, bool xs)
{
    r->held = true;
    r->xs = xs;
    if (xs) {
        r->point++;
    }
    return r->point;
}

/* wl_buffer.release arrived.  True: the buffer is free now.  Ignored for an
 * explicitly synced commit (only its release point counts). */
static inline bool nb_rel_event(struct nb_rel *r)
{
    if (!r->held || r->xs) {
        return false;
    }
    r->held = false;
    return true;
}

/* The release timeline reached `signaled`.  True: the buffer is free now. */
static inline bool nb_rel_signaled(struct nb_rel *r, uint64_t signaled)
{
    if (!r->held || !r->xs || signaled < r->point) {
        return false;
    }
    r->held = false;
    return true;
}

/* The buffer is being destroyed: true when it was still held (whoever owns
 * it must be told it is free, since no release will come any more). */
static inline bool nb_rel_forget(struct nb_rel *r)
{
    bool was = r->held;

    r->held = false;
    r->xs = false;
    return was;
}

/* A release point is outstanding: the syncobj watcher must keep polling. */
static inline bool nb_rel_waiting(const struct nb_rel *r)
{
    return r->held && r->xs;
}

#endif /* NB_PRESENT_H */
