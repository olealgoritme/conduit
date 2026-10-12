/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * nb_scanout.h -- which path a fullscreen frame takes to the screen, and why.
 *
 * Two ways to show a guest frame that is not an exact fit of the output:
 *
 *   ZERO-COPY  the guest's dma-buf itself, on the main surface scaled by a
 *              wp_viewport (picture covers the output) or on the picture
 *              subsurface over black bars (letterbox).  No work in the
 *              viewer.  A compositor that can scale on a plane (or promote
 *              the subsurface to an overlay plane) scans it out directly;
 *              one that cannot composites it.
 *   GPU        one pass of the viewer's own (nb_gpuscale.h) draws the picture
 *              into an output-size buffer allocated for scanout.  Every
 *              compositor can put that shape on the primary plane and tear
 *              it, at the cost of the pass (and a fence wait) per frame.
 *
 * Which one is better depends on the compositor, and the protocols do not
 * say in advance what a compositor will do with a surface: wp_presentation
 * reports it afterwards (KIND_ZERO_COPY per frame, and the present time).  So
 * in fullscreen the viewer MEASURES: it presents a few dozen frames on the
 * zero-copy path; if the compositor scanned them out, that is the answer.  If
 * not, it presents as many through the GPU pass and keeps whichever got
 * frames on screen directly, else the one with the lower receive-to-present
 * time (the GPU pass included), with ties going to zero-copy (no GPU work).
 * The measurement restarts whenever the layout changes (sizes, area, scale,
 * filter, fullscreen, presentation mode).  Nothing here knows any compositor
 * by name.
 *
 * A 1:1 picture (the guest at the output's mode) stays zero-copy, except when
 * the compositor's dma-buf feedback names a scanout tranche that leaves out
 * the guest's format/modifier: then the GPU pass is the copy that gets a
 * plane, and it is measured like the rest.
 *
 * Pure logic, no Wayland: test/test_scanout.c drives it.
 */
#ifndef NB_SCANOUT_H
#define NB_SCANOUT_H

#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

enum nb_so_path { NB_SO_ZC = 0, NB_SO_GPU = 1 };

enum nb_so_phase {
    NB_SO_FIXED = 0,    /* a rule decided; nothing to measure           */
    NB_SO_PROBE_ZC,     /* measuring the zero-copy path                 */
    NB_SO_PROBE_GPU,    /* measuring the GPU pass                       */
    NB_SO_SETTLED,      /* measured; keeps this path until the layout changes */
};

/* Frames per probe, and how long a probe may wait for them. */
#define NB_SO_MIN_SAMPLES   24u
#define NB_SO_FEW_SAMPLES    6u
#define NB_SO_PROBE_NS      1500000000ull
/* Receive-to-present advantage the GPU pass must show to be kept when both
 * paths are composited. */
#define NB_SO_MARGIN_NS     300000ull
#define NB_SO_LAT_N         64u

/* What the decision depends on, filled by the backend for each frame. */
struct nb_so_in {
    bool fullscreen;
    bool gs_ok;            /* the GPU pass can run now (mode, GPU, viewport) */
    const char *gs_off;    /* ...and why not, when it cannot               */
    bool gs_forced;        /* --gpu-scale=on: the pass whenever it applies */
    bool tear_or_latch;    /* immediate or latch: where --gpu-scale=on applies */
    bool exact;            /* buffer == output, no crop, covers the window */
    bool nearest_scaled;   /* Sharp filter on a scaled picture             */
    bool can_measure;      /* wp_presentation with CLOCK_MONOTONIC         */
    int  guest_scanout;    /* feedback: 1 in a scanout tranche, 0 not, -1 unknown */
};

struct nb_so_acc {
    unsigned n, zc;                 /* presented frames, of them zero-copy */
    uint32_t lat_us[NB_SO_LAT_N];   /* receive -> present                  */
    unsigned k;
};

struct nb_so {
    uint64_t key;
    bool     key_valid;
    enum nb_so_phase phase;
    enum nb_so_path  path;
    unsigned gen;                   /* bumped per probe; tags presents     */
    uint64_t t0;
    struct nb_so_acc acc[2];
    bool     changed;               /* path or reason changed: log it      */
    char     why[200];
};

static inline void nb_so_init(struct nb_so *s)
{
    memset(s, 0, sizeof(*s));
    snprintf(s->why, sizeof(s->why), "no frame yet");
}

/* Mix one value into a layout key (FNV-1a over the 8 bytes). */
static inline uint64_t nb_so_mix(uint64_t h, uint64_t v)
{
    if (!h) {
        h = 1469598103934665603ull;
    }
    for (int i = 0; i < 8; i++) {
        h ^= (v >> (8 * i)) & 0xff;
        h *= 1099511628211ull;
    }
    return h;
}

static inline uint32_t nb_so_median_us(const struct nb_so_acc *a)
{
    uint32_t v[NB_SO_LAT_N];
    unsigned n = a->k < NB_SO_LAT_N ? a->k : NB_SO_LAT_N;

    if (!n) {
        return 0;
    }
    memcpy(v, a->lat_us, n * sizeof(v[0]));
    for (unsigned i = 1; i < n; i++) {          /* n <= 64: insertion sort */
        uint32_t x = v[i];
        unsigned j = i;

        while (j > 0 && v[j - 1] > x) {
            v[j] = v[j - 1];
            j--;
        }
        v[j] = x;
    }
    return v[n / 2];
}

static inline bool nb_so_mostly_zc(const struct nb_so_acc *a)
{
    return a->n && a->zc * 2 >= a->n;
}

static inline void nb_so_set(struct nb_so *s, enum nb_so_phase ph,
                             enum nb_so_path p, const char *fmt, ...)
    __attribute__((format(printf, 4, 5)));
static inline void nb_so_set(struct nb_so *s, enum nb_so_phase ph,
                             enum nb_so_path p, const char *fmt, ...)
{
    char why[sizeof(s->why)];
    va_list ap;

    va_start(ap, fmt);
    vsnprintf(why, sizeof(why), fmt, ap);
    va_end(ap);
    if (s->path != p || s->phase != ph || strcmp(s->why, why)) {
        s->changed = true;
    }
    s->phase = ph;
    s->path = p;
    memcpy(s->why, why, sizeof(why));
}

static inline void nb_so_probe(struct nb_so *s, enum nb_so_phase ph,
                               uint64_t now)
{
    s->gen++;
    s->t0 = now;
    if (ph == NB_SO_PROBE_ZC) {
        memset(s->acc, 0, sizeof(s->acc));
        nb_so_set(s, ph, NB_SO_ZC, "measuring the zero-copy path");
    } else {
        memset(&s->acc[NB_SO_GPU], 0, sizeof(s->acc[NB_SO_GPU]));
        nb_so_set(s, ph, NB_SO_GPU, "zero-copy was composited (%u of %u "
                  "frames scanned out); measuring the GPU pass",
                  s->acc[NB_SO_ZC].zc, s->acc[NB_SO_ZC].n);
    }
}

static inline bool nb_so_probe_done(const struct nb_so *s, uint64_t now)
{
    const struct nb_so_acc *a =
        &s->acc[s->phase == NB_SO_PROBE_GPU ? NB_SO_GPU : NB_SO_ZC];

    return a->n >= NB_SO_MIN_SAMPLES ||
           (a->n >= NB_SO_FEW_SAMPLES && now - s->t0 >= NB_SO_PROBE_NS);
}

/* Both probes done: pick. */
static inline void nb_so_settle(struct nb_so *s)
{
    const struct nb_so_acc *a = &s->acc[NB_SO_ZC], *b = &s->acc[NB_SO_GPU];
    uint32_t ma = nb_so_median_us(a), mb = nb_so_median_us(b);

    if (nb_so_mostly_zc(a)) {
        nb_so_set(s, NB_SO_SETTLED, NB_SO_ZC, "the compositor scans "
                  "the guest's buffer out directly (%u of %u frames)",
                  a->zc, a->n);
    } else if (nb_so_mostly_zc(b)) {
        nb_so_set(s, NB_SO_SETTLED, NB_SO_GPU, "only the GPU "
                  "pass's output-size buffer is scanned out directly (%u of "
                  "%u frames; zero-copy %u of %u)", b->zc, b->n, a->zc, a->n);
    } else if (!b->n || (uint64_t)ma * 1000 <= (uint64_t)mb * 1000 +
                                                NB_SO_MARGIN_NS) {
        nb_so_set(s, NB_SO_SETTLED, NB_SO_ZC, "both composited, "
                  "zero-copy reaches the screen as soon (%.2f ms vs %.2f ms "
                  "with the GPU pass), so no GPU pass", ma / 1e3, mb / 1e3);
    } else {
        nb_so_set(s, NB_SO_SETTLED, NB_SO_GPU, "both composited, "
                  "the GPU pass reaches the screen sooner (%.2f ms vs %.2f "
                  "ms zero-copy)", mb / 1e3, ma / 1e3);
    }
}

/*
 * The path for the next commit.  `key` identifies the layout (sizes, area,
 * scale, filter, presentation mode); a new key restarts the measurement.
 */
static inline enum nb_so_path nb_so_decide(struct nb_so *s,
                                           const struct nb_so_in *in,
                                           uint64_t key, uint64_t now)
{
    bool adaptive = false;

    if (!in->fullscreen) {
        nb_so_set(s, NB_SO_FIXED, NB_SO_ZC, "windowed: the compositor places "
                  "the guest's buffer (direct scanout needs fullscreen)");
    } else if (in->exact && in->guest_scanout != 0) {
        nb_so_set(s, NB_SO_FIXED, NB_SO_ZC, "1:1: the guest's buffer covers "
                  "the output as it is");
    } else if (!in->gs_ok) {
        nb_so_set(s, NB_SO_FIXED, NB_SO_ZC, "zero-copy, the compositor "
                  "scales (no GPU pass: %s)",
                  in->gs_off ? in->gs_off : "unavailable");
    } else if (in->gs_forced) {
        bool g = !in->exact && in->tear_or_latch;

        nb_so_set(s, NB_SO_FIXED, g ? NB_SO_GPU : NB_SO_ZC,
                  "--gpu-scale=on%s", g ? "" : ": only with immediate or "
                  "latch presentation");
    } else if (in->nearest_scaled && !in->exact) {
        nb_so_set(s, NB_SO_FIXED, NB_SO_GPU, "Sharp filter: only the "
                  "viewer's own pass scales nearest-neighbour");
    } else if (!in->can_measure) {
        bool g = !in->exact && in->tear_or_latch;

        nb_so_set(s, NB_SO_FIXED, g ? NB_SO_GPU : NB_SO_ZC,
                  "no presentation feedback to measure with: %s",
                  g ? "GPU pass (immediate/latch)" : "zero-copy");
    } else {
        adaptive = true;
    }
    if (!adaptive) {
        s->key_valid = false;
        return s->path;
    }
    if (!s->key_valid || s->key != key) {
        s->key = key;
        s->key_valid = true;
        nb_so_probe(s, NB_SO_PROBE_ZC, now);
        if (in->exact) {
            /* 1:1 outside the scanout tranche: say why it is measured. */
            nb_so_set(s, NB_SO_PROBE_ZC, NB_SO_ZC, "1:1 but the guest's "
                      "format/modifier is not in the scanout tranche; "
                      "measuring zero-copy");
        }
        return s->path;
    }
    if ((s->phase == NB_SO_PROBE_ZC || s->phase == NB_SO_PROBE_GPU) &&
        nb_so_probe_done(s, now)) {
        if (s->phase == NB_SO_PROBE_ZC) {
            if (nb_so_mostly_zc(&s->acc[NB_SO_ZC])) {
                nb_so_settle(s);
            } else {
                nb_so_probe(s, NB_SO_PROBE_GPU, now);
            }
        } else {
            nb_so_settle(s);
        }
    }
    return s->path;
}

/* A present the compositor reported: counted when it belongs to the probe
 * running now (same generation, same path). */
static inline void nb_so_presented(struct nb_so *s, unsigned gen,
                                   enum nb_so_path p, bool zc, uint64_t lat_ns)
{
    struct nb_so_acc *a;

    if (gen != s->gen ||
        !((s->phase == NB_SO_PROBE_ZC && p == NB_SO_ZC) ||
          (s->phase == NB_SO_PROBE_GPU && p == NB_SO_GPU))) {
        return;
    }
    a = &s->acc[p];
    a->n++;
    a->zc += zc ? 1u : 0u;
    if (a->k < NB_SO_LAT_N) {
        uint64_t us = lat_ns / 1000u;

        a->lat_us[a->k++] = us > UINT32_MAX ? UINT32_MAX : (uint32_t)us;
    }
}

/* The path or its reason changed since the last call (for a log line). */
static inline bool nb_so_take_change(struct nb_so *s)
{
    bool c = s->changed;

    s->changed = false;
    return c;
}

static inline const char *nb_so_phase_name(enum nb_so_phase p)
{
    switch (p) {
    case NB_SO_PROBE_ZC:
    case NB_SO_PROBE_GPU: return "measuring";
    case NB_SO_SETTLED:   return "measured";
    default:              return "rule";
    }
}

/* ── the compositor's scanout tranche (dma-buf feedback v4) ──────────── */

#define NB_SO_FMTS 128
struct nb_so_fmts {
    struct { uint32_t fourcc; uint64_t mod; } e[NB_SO_FMTS];
    unsigned n;
    bool overflowed;
};

static inline bool nb_so_fmts_has(const struct nb_so_fmts *f, uint32_t fourcc,
                                  uint64_t mod)
{
    for (unsigned i = 0; i < f->n; i++) {
        if (f->e[i].fourcc == fourcc && f->e[i].mod == mod) {
            return true;
        }
    }
    return false;
}

static inline void nb_so_fmts_add(struct nb_so_fmts *f, uint32_t fourcc,
                                  uint64_t mod)
{
    if (nb_so_fmts_has(f, fourcc, mod)) {
        return;
    }
    if (f->n >= NB_SO_FMTS) {
        f->overflowed = true;
        return;
    }
    f->e[f->n].fourcc = fourcc;
    f->e[f->n].mod = mod;
    f->n++;
}

/* Modifiers listed for `fourcc`, at most `max`; returns how many. */
static inline unsigned nb_so_fmts_mods(const struct nb_so_fmts *f,
                                       uint32_t fourcc, uint64_t *out,
                                       unsigned max)
{
    unsigned k = 0;

    for (unsigned i = 0; i < f->n && k < max; i++) {
        if (f->e[i].fourcc == fourcc) {
            out[k++] = f->e[i].mod;
        }
    }
    return k;
}

/*
 * Whether a (fourcc, mod) can be scanned out, per the feedback: 1 listed in
 * a scanout tranche, 0 a scanout tranche exists and does not list it (an
 * XRGB/ARGB twin counts: a plane ignores the alpha of an opaque surface),
 * -1 the compositor named no scanout tranche (unknown).
 */
static inline int nb_so_guest_scanout(const struct nb_so_fmts *scan,
                                      bool have_tranche, uint32_t fourcc,
                                      uint32_t twin, uint64_t mod)
{
    if (!have_tranche) {
        return -1;
    }
    return nb_so_fmts_has(scan, fourcc, mod) ||
           (twin && nb_so_fmts_has(scan, twin, mod)) ? 1 : 0;
}

#endif /* NB_SCANOUT_H */
