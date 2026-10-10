/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * nb_view.h -- where the guest's picture goes in the viewer window, and how a
 * pointer position in the window maps back into the guest.
 *
 * Pure arithmetic and a small settings file, no display-server code, so every
 * rule here is checked by test/test_view.c without a compositor.
 *
 * Four settings, gamescope-style:
 *
 *   guest resolution  native (the output area, in output pixels) or a fixed
 *                     WxH the viewer asks the guest for with the mode hint
 *   output area       the whole window, an aspect preset (21:9, 16:9, 16:10,
 *                     4:3) as large as fits, or a custom WxH (+X+Y, centred
 *                     when no offset is given), in output pixels
 *   scale             fit (aspect kept, black bars), stretch, integer, none
 *   filter            linear or nearest
 *
 * Window coordinates are the backend's own: logical pixels on Wayland (what
 * wl_pointer and wp_viewport speak), real pixels on X11.  Output pixels are
 * window pixels times s120/120 -- the output's (fractional) scale.  The
 * placement is computed in output pixels, so integer and 1:1 mean what they
 * say on a HiDPI output, then expressed in window coordinates.  The pointer is
 * mapped through the SAME window-coordinate rectangle the picture is drawn
 * into, so the mapping is the exact inverse of the draw whatever the rounding.
 */
#ifndef NB_VIEW_H
#define NB_VIEW_H

#include <stdbool.h>
#include <stddef.h>

/* Scale modes.  The first three keep the values nb_config.scale_mode always
 * had (nvkvm_broker.h defines the same names). */
#ifndef NB_SCALE_NONE
#define NB_SCALE_NONE    0      /* 1:1 in output pixels, centred (cropped if larger) */
#define NB_SCALE_STRETCH 1      /* fill the area, aspect ignored             */
#define NB_SCALE_ASPECT  2      /* fit: as large as fits, aspect kept, bars  */
#endif
#define NB_SCALE_INTEGER 3      /* largest whole multiple that fits; fit if none */
#define NB_SCALE_COUNT   4

#define NB_FILTER_LINEAR  0
#define NB_FILTER_NEAREST 1

enum {
    NB_AREA_FULL = 0,
    NB_AREA_21_9,
    NB_AREA_16_9,
    NB_AREA_16_10,
    NB_AREA_4_3,
    NB_AREA_CUSTOM,
    NB_AREA_COUNT
};

/* Guest resolution presets after "native"; Ctrl+Alt+R walks these, then the
 * last custom one, then back to native. */
#define NB_RES_PRESETS 4
extern const unsigned nb_res_preset[NB_RES_PRESETS][2];

struct nb_view {
    int      scale;             /* NB_SCALE_*                                */
    int      filter;            /* NB_FILTER_*                               */
    int      area;              /* NB_AREA_*                                 */
    int      cw, ch;            /* custom area size, output pixels           */
    int      cx, cy;            /* custom area offset, output pixels         */
    bool     ccenter;           /* custom area centred (no +X+Y given)       */
    unsigned res_w, res_h;      /* guest resolution; 0x0 = native            */
};

struct nb_rect { int x, y, w, h; };

struct nb_place {
    struct nb_rect area;        /* the output area, window coordinates       */
    struct nb_rect pic;         /* the whole picture; may exceed the area    */
    struct nb_rect vis;         /* pic clipped to the area: what is drawn    */
    double sx, sy, sw, sh;      /* the buffer rectangle shown in vis         */
    bool   cropped;             /* vis is smaller than pic                   */
};

void nb_view_defaults(struct nb_view *v);

/* The output area for a ww x wh window, in window coordinates. */
void nb_view_area(const struct nb_view *v, int ww, int wh, unsigned s120,
                  struct nb_rect *out);
/* The same in output pixels (what a custom area is expressed in). */
void nb_view_area_px(const struct nb_view *v, int ww, int wh, unsigned s120,
                     struct nb_rect *out);
/* Where a bw x bh guest picture goes. */
void nb_view_place(const struct nb_view *v, int bw, int bh, int ww, int wh,
                   unsigned s120, struct nb_place *p);
/*
 * Window position -> guest pixel.  The point is clamped to the visible
 * picture first, so over the bars the guest pointer rests on the nearest edge.
 * Returns true when the point was over the picture itself.  gx/gy are in
 * 0..bw-1 / 0..bh-1; send them with the range bw x bh.
 */
bool nb_view_map(const struct nb_place *p, int bw, int bh, double wx,
                 double wy, int *gx, int *gy);
/* The guest pixel -> window scale, per axis (for drawing the guest cursor). */
void nb_view_cursor_scale(const struct nb_place *p, int bw, int bh,
                          double *kx, double *ky);
/* The mode to ask the guest for: the fixed resolution, or else the output
 * area in output pixels.  Returns true for a fixed one. */
bool nb_view_guest_mode(const struct nb_view *v, int ww, int wh, unsigned s120,
                        unsigned *pw, unsigned *ph);

/* ── names ──────────────────────────────────────────────────────────────── */

/* Each returns 0 and fills in the field(s), or -1 for something unknown. */
int nb_view_parse_scale(const char *s, int *scale);   /* fit|aspect|stretch|integer|none|centered */
int nb_view_parse_filter(const char *s, int *filter); /* linear|nearest (smooth|sharp) */
int nb_view_parse_area(const char *s, struct nb_view *v); /* full|21:9|16:9|16:10|4:3|WxH[+X+Y] */
int nb_view_parse_res(const char *s, unsigned *w, unsigned *h); /* native|WxH */

const char *nb_view_scale_name(int scale);      /* "fit", "stretch", ...     */
const char *nb_view_scale_label(int scale);     /* "Fit", "Stretch", ...     */
const char *nb_view_filter_name(int filter);    /* "linear", "nearest"       */
const char *nb_view_area_label(int area);       /* "Full window", "16:9", .. */
void nb_view_area_str(const struct nb_view *v, char *buf, size_t n);  /* settings-file form */
void nb_view_res_str(const struct nb_view *v, char *buf, size_t n);   /* "native" / "1920x1080" */

/* ── saved settings, profiles ───────────────────────────────────────────── */

#define NB_MAX_PROFILES   16
#define NB_PROFILE_NAME   40
#define NB_MAX_RECENT     4

struct nb_profile {
    char           name[NB_PROFILE_NAME];
    struct nb_view v;
};

struct nb_vstore {
    struct nb_view    cur;
    struct nb_profile prof[NB_MAX_PROFILES];
    int               nprof;
    int               prof_at;                  /* last loaded, -1 none  */
    unsigned          recent[NB_MAX_RECENT][2]; /* custom resolutions, newest first */
    int               nrecent;
};

void nb_vstore_init(struct nb_vstore *st);
/* 0 on success (missing file = defaults, success), -1 on a read error.
 * Unknown or malformed lines are skipped. */
int nb_vstore_load(struct nb_vstore *st, const char *path);
/* Written to path.tmp and renamed.  0 or -errno. */
int nb_vstore_save(const struct nb_vstore *st, const char *path);
/* Parse / format a whole settings text (what load/save use). */
void nb_vstore_parse(struct nb_vstore *st, const char *text);
size_t nb_vstore_format(const struct nb_vstore *st, char *buf, size_t n);

/* Remember a custom resolution (not a preset) at the front of `recent`. */
void nb_vstore_note_res(struct nb_vstore *st, unsigned w, unsigned h);
/* Save cur under name (replacing one of the same name).  Index or -1. */
int  nb_vstore_profile_save(struct nb_vstore *st, const char *name);
int  nb_vstore_profile_load(struct nb_vstore *st, int i);
void nb_vstore_profile_delete(struct nb_vstore *st, int i);

/* ── hotkey actions ─────────────────────────────────────────────────────── */
/*
 * Each changes st->cur and writes a short label for the on-screen notice.
 * They return NB_VIEW_CH_* bits: what the backend has to redo.
 */
#define NB_VIEW_CH_LAYOUT 1u    /* re-place the picture                      */
#define NB_VIEW_CH_RES    2u    /* re-send the mode hint                     */

unsigned nb_view_cycle_scale(struct nb_vstore *st, char *label, size_t n);
unsigned nb_view_cycle_filter(struct nb_vstore *st, char *label, size_t n);
unsigned nb_view_cycle_area(struct nb_vstore *st, char *label, size_t n);
unsigned nb_view_cycle_res(struct nb_vstore *st, char *label, size_t n);
unsigned nb_view_cycle_profile(struct nb_vstore *st, char *label, size_t n);
/* Move (dx, dy) or grow (dw, keeping the aspect) the area, in output pixels;
 * a preset area becomes a custom one where it is now. */
unsigned nb_view_nudge(struct nb_vstore *st, int ww, int wh, unsigned s120,
                       int dx, int dy, int dw, char *label, size_t n);
unsigned nb_view_reset(struct nb_vstore *st, char *label, size_t n);
/* The one-line summary: "FIT · 16:9 · 1920x1080 · LINEAR" */
void nb_view_summary(const struct nb_view *v, char *buf, size_t n);

/* Command-line overrides: which fields the user gave, applied over whatever
 * the settings file restored. */
#define NB_VIEW_SET_SCALE  1u
#define NB_VIEW_SET_FILTER 2u
#define NB_VIEW_SET_AREA   4u
#define NB_VIEW_SET_RES    8u
void nb_view_override(struct nb_view *dst, const struct nb_view *src,
                      unsigned mask);

#endif /* NB_VIEW_H */
