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

/*
 * Presentation sync: when the viewer asks the compositor to show a frame the
 * moment it is committed (tearing, wp_tearing_control_v1 ASYNC) rather than
 * at the next vblank.  Not part of a layout or a profile: one setting per VM.
 */
enum {
    NB_SYNC_FULLSCREEN = 0,     /* default: immediate in fullscreen, vsync windowed */
    NB_SYNC_IMMEDIATE,          /* immediate in a window too                 */
    NB_SYNC_VSYNC,              /* always wait for vblank                    */
    NB_SYNC_LATCH,              /* newest frame once per refresh, late latch
                                 * (nb_latch.h)                              */
    NB_SYNC_COUNT
};
int         nb_sync_parse(const char *s, int *sync);  /* fullscreen|auto|immediate|vsync|latch */
const char *nb_sync_name(int sync);                   /* settings-file / flag form */
const char *nb_sync_label(int sync);                  /* menu form               */
/* Whether frames should be presented immediately in this state. */
bool        nb_sync_immediate(int sync, bool fullscreen);

/*
 * Picture enhancement (docs/VIEWER-ENHANCE.md): an upscaler or sharpener in
 * the viewer's GPU pass, and host-side frame generation.  Like sync, one
 * setting per VM, not part of a layout or a profile; all off by default.
 */
enum {
    NB_UPSCALE_OFF = 0,         /* the plain bilinear/nearest pass (or none)  */
    NB_UPSCALE_FSR,             /* AMD FidelityFX FSR 1: EASU + RCAS         */
    NB_UPSCALE_NIS,             /* NVIDIA Image Scaling                      */
    NB_UPSCALE_CAS,             /* AMD FidelityFX CAS: sharpen, plain scale  */
    NB_UPSCALE_COUNT
};
enum {
    NB_FGEN_OFF = 0,
    NB_FGEN_2X,                 /* one generated frame per guest frame       */
    NB_FGEN_3X,                 /* two                                       */
    NB_FGEN_NVSM,               /* NVIDIA Smooth Motion through a Vulkan
                                 * swapchain (RTX 40/50 hosts)               */
    NB_FGEN_COUNT
};
#define NB_SHARP_DEFAULT 50     /* sharpness, 0..100                         */
int         nb_upscale_parse(const char *s, int *up);  /* off|fsr|nis|cas */
const char *nb_upscale_name(int up);                   /* settings-file form */
const char *nb_upscale_label(int up);                  /* menu form          */
int         nb_fgen_parse(const char *s, int *fg);     /* off|2x|3x|nvidia   */
const char *nb_fgen_name(int fg);
const char *nb_fgen_label(int fg);
/* Frames shown per guest frame by the viewer's own generation: 2 or 3, and
 * 1 for off and for Smooth Motion (the driver generates those). */
int         nb_fgen_mult(int fg);
/* 0..100, from a settings-file value; -1 for junk. */
int         nb_sharp_parse(const char *s);

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
/*
 * True when the drawn picture leaves part of a ww x wh window uncovered:
 * the bars, which the backend fills with an opaque black background.  False
 * only for an exact fit, where the picture alone covers the window.
 */
bool nb_view_has_bars(const struct nb_place *p, int ww, int wh);
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
#define NB_MAX_RULES      16

struct nb_profile {
    char           name[NB_PROFILE_NAME];
    struct nb_view v;
};

/*
 * A per-guest-mode rule: while the guest runs at w x h, the picture uses v's
 * area, scale and filter (its res is not used).  "1280x960 -> 4:3 area,
 * stretch"; the native mode can have one too ("full, 1:1").
 */
struct nb_rule {
    unsigned       w, h;
    struct nb_view v;
};

struct nb_vstore {
    struct nb_view    cur;
    int               sync;                     /* NB_SYNC_*             */
    int               upscale;                  /* NB_UPSCALE_*          */
    int               sharp;                    /* 0..100                */
    int               fgen;                     /* NB_FGEN_*             */
    struct nb_profile prof[NB_MAX_PROFILES];
    int               nprof;
    int               prof_at;                  /* last loaded, -1 none  */
    unsigned          recent[NB_MAX_RECENT][2]; /* custom resolutions, newest first */
    int               nrecent;
    struct nb_rule    rule[NB_MAX_RULES];
    int               nrule;
    /*
     * A rule is in force (the guest runs at rule_w x rule_h): cur holds the
     * rule's layout, base the layout to go back to when the guest leaves
     * that mode for one without a rule.  The file stores base.
     */
    bool              ruled;
    unsigned          rule_w, rule_h;
    struct nb_view    base;
    /* base (the default for modes without a rule) is known: taken from
     * cur when the first guest mode arrives, so the file's settings and any
     * command-line overrides are the default. */
    bool              base_set;
    /* The guest mode the picture is shown at now, 0x0 before the first. */
    unsigned          mode_w, mode_h;
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

/* Per-guest-mode rules.  find: index or -1.  save: cur's area, scale and
 * filter for w x h (replacing one for the same mode); index or -1 when full.
 * After a save the rule is in force (the guest runs at that mode). */
int  nb_vstore_rule_find(const struct nb_vstore *st, unsigned w, unsigned h);
int  nb_vstore_rule_save(struct nb_vstore *st, unsigned w, unsigned h);
void nb_vstore_rule_delete(struct nb_vstore *st, int i);
/*
 * The guest's picture is now w x h: apply the rule for that mode (keeping
 * the layout to come back to), or go back to that layout when it has none.
 * Returns NB_VIEW_CH_LAYOUT when cur's layout changed, with a notice label.
 */
unsigned nb_vstore_guest_mode(struct nb_vstore *st, unsigned w, unsigned h,
                              char *label, size_t n);

/*
 * AUTOMATIC PER-MODE RULES.  Call after any change of cur's layout (scale,
 * area, custom area, filter): the layout is remembered for the guest mode in
 * force (mode_w x mode_h), or that mode's rule is dropped when the layout is
 * the default again.  The oldest rule makes room when the table is full.
 * Returns true when the store changed (persist it).  Before the first guest
 * mode nothing is stored: cur then becomes the default.
 */
bool nb_vstore_auto_rule(struct nb_vstore *st);
/* cur's layout becomes the default for every mode without a rule; the
 * current mode's own rule goes. */
void nb_vstore_make_default(struct nb_vstore *st);

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

/* ── the display's mode list (CMD_MODES, docs/SCANOUT.md "Mode list") ──── */
/*
 * The modes the guest is offered, as the backend sends them: native first,
 * the standard modes up to it, the VM's custom modes.  One list per VM: the
 * menu's resolution list and Ctrl+Alt+R walk exactly this.
 */
#define NB_MODES_MAX     64
#define NB_MODE_NATIVE   1u
#define NB_MODE_CUSTOM   2u

struct nb_modes {
    unsigned           n;               /* 0: none (an older backend)   */
    unsigned           w[NB_MODES_MAX], h[NB_MODES_MAX];
    unsigned           flags[NB_MODES_MAX];
    unsigned           mhz;
    unsigned long long gen;
};

/* Reassembles the records of one list; `cur` is the last complete one. */
struct nb_modes_rx {
    struct nb_modes cur;
    struct nb_modes in;
    unsigned        next;               /* index expected next          */
};

/*
 * One CMD_MODES record.  1: it completed a list, now in rx->cur; 0: more to
 * come; -1: malformed (bad size or count, out of order, mixed generations):
 * the list in progress is dropped, rx->cur is kept.
 */
int nb_modes_feed(struct nb_modes_rx *rx, unsigned w, unsigned h, unsigned idx,
                  unsigned count, unsigned mhz, unsigned long long gen,
                  unsigned flags);
/* Index of w x h in m, or -1. */
int nb_modes_find(const struct nb_modes *m, unsigned w, unsigned h);
/* Ctrl+Alt+R with a list: the guest resolution after the one the guest runs
 * at (cur_w x cur_h; the list's first when it is not in it), wrapping. */
unsigned nb_view_cycle_mode(struct nb_vstore *st, const struct nb_modes *m,
                            unsigned cur_w, unsigned cur_h, char *label,
                            size_t n);

/* Command-line overrides: which fields the user gave, applied over whatever
 * the settings file restored. */
#define NB_VIEW_SET_SCALE  1u
#define NB_VIEW_SET_FILTER 2u
#define NB_VIEW_SET_AREA   4u
#define NB_VIEW_SET_RES    8u
void nb_view_override(struct nb_view *dst, const struct nb_view *src,
                      unsigned mask);

#endif /* NB_VIEW_H */
