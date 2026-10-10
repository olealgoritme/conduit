/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * nb_view.c -- picture placement, pointer mapping and the viewer's saved
 * display settings.  See nb_view.h.
 */
#include "nb_view.h"

#include <errno.h>
#include <fcntl.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

const unsigned nb_res_preset[NB_RES_PRESETS][2] = {
    { 2560, 1440 }, { 1920, 1080 }, { 1600, 900 }, { 1280, 960 },
};

static const int area_aspect[NB_AREA_COUNT][2] = {
    [NB_AREA_21_9]  = { 21, 9 },
    [NB_AREA_16_9]  = { 16, 9 },
    [NB_AREA_16_10] = { 16, 10 },
    [NB_AREA_4_3]   = { 4, 3 },
};

#define NB_VIEW_MAX_DIM 16384

void nb_view_defaults(struct nb_view *v)
{
    memset(v, 0, sizeof(*v));
    v->scale = NB_SCALE_ASPECT;
    v->filter = NB_FILTER_LINEAR;
    v->area = NB_AREA_FULL;
    v->ccenter = true;
}

static unsigned s_or_1(unsigned s120)
{
    return s120 ? s120 : 120;
}

/* window -> output pixels */
static int to_px(int l, unsigned s120)
{
    return (int)(((long)l * s_or_1(s120) + 60) / 120);
}

/* output pixels -> window, rounding an EDGE (so x and x+w round alike) */
static int to_win(int p, unsigned s120)
{
    long s = s_or_1(s120);

    if (p >= 0) {
        return (int)(((long)p * 120 + s / 2) / s);
    }
    return -(int)(((long)-p * 120 + s / 2) / s);
}

static void rect_to_win(const struct nb_rect *px, unsigned s120,
                        struct nb_rect *out)
{
    int x0 = to_win(px->x, s120), y0 = to_win(px->y, s120);
    int x1 = to_win(px->x + px->w, s120), y1 = to_win(px->y + px->h, s120);

    out->x = x0;
    out->y = y0;
    out->w = x1 - x0 > 0 ? x1 - x0 : 1;
    out->h = y1 - y0 > 0 ? y1 - y0 : 1;
}

/* The largest bw:bh rectangle inside aw x ah, with the same snapping rule as
 * nb_fit(): within a pixel of the container counts as equal, so a matching
 * aspect never leaves a 1 px bar (which would cost direct scanout). */
static void fit_aspect(long bw, long bh, int aw, int ah, int *w, int *h)
{
    long by_h = (long)ah * bw / bh;

    if (by_h <= aw) {
        *w = (int)by_h;
        *h = ah;
    } else {
        *w = aw;
        *h = (int)((long)aw * bh / bw);
    }
    if (*w > aw - 2 && *w < aw + 2) {
        *w = aw;
    }
    if (*h > ah - 2 && *h < ah + 2) {
        *h = ah;
    }
    if (*w < 1) {
        *w = 1;
    }
    if (*h < 1) {
        *h = 1;
    }
}

void nb_view_area_px(const struct nb_view *v, int ww, int wh, unsigned s120,
                     struct nb_rect *out)
{
    int W = to_px(ww > 0 ? ww : 1, s120), H = to_px(wh > 0 ? wh : 1, s120);
    int a = v->area;

    out->x = 0;
    out->y = 0;
    out->w = W;
    out->h = H;
    if (a > NB_AREA_FULL && a < NB_AREA_CUSTOM) {
        fit_aspect(area_aspect[a][0], area_aspect[a][1], W, H, &out->w,
                   &out->h);
        out->x = (W - out->w) / 2;
        out->y = (H - out->h) / 2;
    } else if (a == NB_AREA_CUSTOM && v->cw > 0 && v->ch > 0) {
        out->w = v->cw < W ? v->cw : W;
        out->h = v->ch < H ? v->ch : H;
        if (v->ccenter) {
            out->x = (W - out->w) / 2;
            out->y = (H - out->h) / 2;
        } else {
            out->x = v->cx < 0 ? 0 : v->cx;
            out->y = v->cy < 0 ? 0 : v->cy;
            if (out->x + out->w > W) {
                out->x = W - out->w;
            }
            if (out->y + out->h > H) {
                out->y = H - out->h;
            }
        }
    }
}

void nb_view_area(const struct nb_view *v, int ww, int wh, unsigned s120,
                  struct nb_rect *out)
{
    struct nb_rect px;

    if (v->area == NB_AREA_FULL) {
        /* Exactly the window, never a rounding of it. */
        out->x = 0;
        out->y = 0;
        out->w = ww > 0 ? ww : 1;
        out->h = wh > 0 ? wh : 1;
        return;
    }
    nb_view_area_px(v, ww, wh, s120, &px);
    rect_to_win(&px, s120, out);
    /* Never outside the window after rounding. */
    if (out->x < 0) { out->x = 0; }
    if (out->y < 0) { out->y = 0; }
    if (ww > 0 && out->x + out->w > ww) { out->w = ww - out->x; }
    if (wh > 0 && out->y + out->h > wh) { out->h = wh - out->y; }
}

void nb_view_place(const struct nb_view *v, int bw, int bh, int ww, int wh,
                   unsigned s120, struct nb_place *p)
{
    struct nb_rect apx, ppx = { 0, 0, 1, 1 };
    int mode = v->scale;

    memset(p, 0, sizeof(*p));
    if (ww <= 0) { ww = bw > 0 ? bw : 1; }
    if (wh <= 0) { wh = bh > 0 ? bh : 1; }
    if (bw <= 0 || bh <= 0) {
        bw = ww;
        bh = wh;
    }
    nb_view_area(v, ww, wh, s120, &p->area);
    nb_view_area_px(v, ww, wh, s120, &apx);
    if (v->area == NB_AREA_FULL) {
        apx.w = to_px(ww, s120);
        apx.h = to_px(wh, s120);
    }

    if (mode == NB_SCALE_INTEGER) {
        int k = apx.w / bw < apx.h / bh ? apx.w / bw : apx.h / bh;

        if (k >= 1) {
            ppx.w = bw * k;
            ppx.h = bh * k;
        } else {
            mode = NB_SCALE_ASPECT;     /* larger than the area: fit it */
        }
    }
    if (mode == NB_SCALE_STRETCH) {
        p->pic = p->area;
    } else if (mode == NB_SCALE_ASPECT) {
        int w, h;

        /* In window coordinates directly: an aspect fit that fills one axis
         * must fill it exactly, and the area already is exact. */
        fit_aspect(bw, bh, p->area.w, p->area.h, &w, &h);
        p->pic.w = w;
        p->pic.h = h;
        p->pic.x = p->area.x + (p->area.w - w) / 2;
        p->pic.y = p->area.y + (p->area.h - h) / 2;
    } else {
        struct nb_rect r;

        if (mode == NB_SCALE_NONE) {
            ppx.w = bw;
            ppx.h = bh;
        }
        /* Centred in output pixels, then to window coordinates. */
        r.x = apx.x + (apx.w - ppx.w) / 2;
        r.y = apx.y + (apx.h - ppx.h) / 2;
        r.w = ppx.w;
        r.h = ppx.h;
        if (r.w == apx.w && r.h == apx.h) {
            p->pic = p->area;   /* exactly the area: no rounding drift */
        } else {
            rect_to_win(&r, s120, &p->pic);
        }
    }

    /* What is visible: the picture clipped to the area. */
    {
        int x0 = p->pic.x > p->area.x ? p->pic.x : p->area.x;
        int y0 = p->pic.y > p->area.y ? p->pic.y : p->area.y;
        int x1 = p->pic.x + p->pic.w < p->area.x + p->area.w
                     ? p->pic.x + p->pic.w : p->area.x + p->area.w;
        int y1 = p->pic.y + p->pic.h < p->area.y + p->area.h
                     ? p->pic.y + p->pic.h : p->area.y + p->area.h;

        if (x1 <= x0) { x1 = x0 + 1; }
        if (y1 <= y0) { y1 = y0 + 1; }
        p->vis.x = x0;
        p->vis.y = y0;
        p->vis.w = x1 - x0;
        p->vis.h = y1 - y0;
    }
    p->cropped = p->vis.w != p->pic.w || p->vis.h != p->pic.h;
    p->sx = (double)(p->vis.x - p->pic.x) * bw / p->pic.w;
    p->sy = (double)(p->vis.y - p->pic.y) * bh / p->pic.h;
    p->sw = (double)p->vis.w * bw / p->pic.w;
    p->sh = (double)p->vis.h * bh / p->pic.h;
}

bool nb_view_map(const struct nb_place *p, int bw, int bh, double wx,
                 double wy, int *gx, int *gy)
{
    double x0 = p->vis.x, y0 = p->vis.y;
    double x1 = p->vis.x + p->vis.w, y1 = p->vis.y + p->vis.h;
    bool inside = wx >= x0 && wx < x1 && wy >= y0 && wy < y1;
    double fx, fy;
    long ix, iy;

    if (bw <= 0 || bh <= 0 || p->pic.w <= 0 || p->pic.h <= 0) {
        *gx = *gy = 0;
        return false;
    }
    /* Clamp to the visible picture: over a bar, rest on the nearest edge. */
    if (wx < x0) { wx = x0; }
    if (wy < y0) { wy = y0; }
    if (wx >= x1) { wx = x1 - 1e-6; }
    if (wy >= y1) { wy = y1 - 1e-6; }
    /* The inverse of the draw: window -> picture -> buffer. */
    fx = (wx - p->pic.x) * bw / p->pic.w;
    fy = (wy - p->pic.y) * bh / p->pic.h;
    ix = (long)floor(fx);
    iy = (long)floor(fy);
    if (ix < 0) { ix = 0; }
    if (iy < 0) { iy = 0; }
    if (ix > bw - 1) { ix = bw - 1; }
    if (iy > bh - 1) { iy = bh - 1; }
    *gx = (int)ix;
    *gy = (int)iy;
    return inside;
}

void nb_view_cursor_scale(const struct nb_place *p, int bw, int bh,
                          double *kx, double *ky)
{
    *kx = bw > 0 && p->pic.w > 0 ? (double)p->pic.w / bw : 1.0;
    *ky = bh > 0 && p->pic.h > 0 ? (double)p->pic.h / bh : 1.0;
}

bool nb_view_guest_mode(const struct nb_view *v, int ww, int wh, unsigned s120,
                        unsigned *pw, unsigned *ph)
{
    struct nb_rect a;

    if (v->res_w && v->res_h) {
        *pw = v->res_w;
        *ph = v->res_h;
        return true;
    }
    if (v->area == NB_AREA_FULL) {
        a.w = to_px(ww, s120);
        a.h = to_px(wh, s120);
    } else {
        nb_view_area_px(v, ww, wh, s120, &a);
    }
    *pw = a.w > 0 ? (unsigned)a.w : 0;
    *ph = a.h > 0 ? (unsigned)a.h : 0;
    return false;
}

/* ── names ──────────────────────────────────────────────────────────────── */

int nb_view_parse_scale(const char *s, int *scale)
{
    if (!s) {
        return -1;
    }
    if (!strcmp(s, "fit") || !strcmp(s, "aspect")) {
        *scale = NB_SCALE_ASPECT;
    } else if (!strcmp(s, "stretch")) {
        *scale = NB_SCALE_STRETCH;
    } else if (!strcmp(s, "integer")) {
        *scale = NB_SCALE_INTEGER;
    } else if (!strcmp(s, "none") || !strcmp(s, "centered") ||
               !strcmp(s, "centred")) {
        *scale = NB_SCALE_NONE;
    } else {
        return -1;
    }
    return 0;
}

int nb_view_parse_filter(const char *s, int *filter)
{
    if (!s) {
        return -1;
    }
    if (!strcmp(s, "linear") || !strcmp(s, "smooth") ||
        !strcmp(s, "bilinear")) {
        *filter = NB_FILTER_LINEAR;
    } else if (!strcmp(s, "nearest") || !strcmp(s, "sharp")) {
        *filter = NB_FILTER_NEAREST;
    } else {
        return -1;
    }
    return 0;
}

static int parse_dim(const char *s, unsigned *w, unsigned *h, const char **end)
{
    char *e;
    unsigned long a, b;

    if (*s < '0' || *s > '9') {
        return -1;
    }
    a = strtoul(s, &e, 10);
    if (*e != 'x' && *e != 'X') {
        return -1;
    }
    s = e + 1;
    if (*s < '0' || *s > '9') {
        return -1;
    }
    b = strtoul(s, &e, 10);
    if (!a || !b || a > NB_VIEW_MAX_DIM || b > NB_VIEW_MAX_DIM) {
        return -1;
    }
    *w = (unsigned)a;
    *h = (unsigned)b;
    *end = e;
    return 0;
}

int nb_view_parse_area(const char *s, struct nb_view *v)
{
    static const char *const names[NB_AREA_CUSTOM] = {
        "full", "21:9", "16:9", "16:10", "4:3",
    };
    unsigned w, h;
    const char *e;
    int i;

    if (!s) {
        return -1;
    }
    for (i = 0; i < NB_AREA_CUSTOM; i++) {
        if (!strcmp(s, names[i])) {
            v->area = i;
            return 0;
        }
    }
    if (!strcmp(s, "window")) {
        v->area = NB_AREA_FULL;
        return 0;
    }
    if (parse_dim(s, &w, &h, &e) != 0) {
        return -1;
    }
    if (*e == '\0') {
        v->area = NB_AREA_CUSTOM;
        v->cw = (int)w;
        v->ch = (int)h;
        v->ccenter = true;
        v->cx = v->cy = 0;
        return 0;
    }
    {
        char *e2;
        long x, y;

        if (*e != '+') {
            return -1;
        }
        x = strtol(e + 1, &e2, 10);
        if (e2 == e + 1 || *e2 != '+') {
            return -1;
        }
        e = e2;
        y = strtol(e + 1, &e2, 10);
        if (e2 == e + 1 || *e2 != '\0' || x < 0 || y < 0 ||
            x > NB_VIEW_MAX_DIM || y > NB_VIEW_MAX_DIM) {
            return -1;
        }
        v->area = NB_AREA_CUSTOM;
        v->cw = (int)w;
        v->ch = (int)h;
        v->cx = (int)x;
        v->cy = (int)y;
        v->ccenter = false;
    }
    return 0;
}

int nb_view_parse_res(const char *s, unsigned *w, unsigned *h)
{
    const char *e;
    unsigned a, b;

    if (!s) {
        return -1;
    }
    if (!strcmp(s, "native") || !strcmp(s, "auto")) {
        *w = *h = 0;
        return 0;
    }
    if (parse_dim(s, &a, &b, &e) != 0 || *e) {
        return -1;
    }
    *w = a;
    *h = b;
    return 0;
}

const char *nb_view_scale_name(int scale)
{
    switch (scale) {
    case NB_SCALE_STRETCH: return "stretch";
    case NB_SCALE_INTEGER: return "integer";
    case NB_SCALE_NONE:    return "none";
    default:               return "fit";
    }
}

const char *nb_view_scale_label(int scale)
{
    switch (scale) {
    case NB_SCALE_STRETCH: return "Stretch";
    case NB_SCALE_INTEGER: return "Integer";
    case NB_SCALE_NONE:    return "Centered";
    default:               return "Fit";
    }
}

const char *nb_view_filter_name(int filter)
{
    return filter == NB_FILTER_NEAREST ? "nearest" : "linear";
}

const char *nb_view_area_label(int area)
{
    switch (area) {
    case NB_AREA_21_9:   return "21:9";
    case NB_AREA_16_9:   return "16:9";
    case NB_AREA_16_10:  return "16:10";
    case NB_AREA_4_3:    return "4:3";
    case NB_AREA_CUSTOM: return "Custom";
    default:             return "Full window";
    }
}

void nb_view_area_str(const struct nb_view *v, char *buf, size_t n)
{
    static const char *const names[NB_AREA_CUSTOM] = {
        "full", "21:9", "16:9", "16:10", "4:3",
    };

    if (v->area == NB_AREA_CUSTOM && v->cw > 0 && v->ch > 0) {
        if (v->ccenter) {
            snprintf(buf, n, "%dx%d", v->cw, v->ch);
        } else {
            snprintf(buf, n, "%dx%d+%d+%d", v->cw, v->ch, v->cx, v->cy);
        }
    } else {
        snprintf(buf, n, "%s",
                 v->area >= 0 && v->area < NB_AREA_CUSTOM ? names[v->area]
                                                          : "full");
    }
}

void nb_view_res_str(const struct nb_view *v, char *buf, size_t n)
{
    if (v->res_w && v->res_h) {
        snprintf(buf, n, "%ux%u", v->res_w, v->res_h);
    } else {
        snprintf(buf, n, "native");
    }
}

void nb_view_summary(const struct nb_view *v, char *buf, size_t n)
{
    char a[40], r[24];

    nb_view_area_str(v, a, sizeof(a));
    nb_view_res_str(v, r, sizeof(r));
    snprintf(buf, n, "%s | %s | %s | %s", nb_view_scale_label(v->scale),
             v->area == NB_AREA_FULL ? "Full window" : a, r,
             v->filter == NB_FILTER_NEAREST ? "sharp" : "smooth");
}

/* ── the settings file ──────────────────────────────────────────────────── */
/*
 * Plain text, one setting per line, so it can be read and edited by hand:
 *
 *   scale=fit
 *   filter=linear
 *   area=16:9
 *   res=native
 *   recent=1920x1080
 *   profile=CS2 4:3 stretched|scale=stretch area=4:3 res=1280x960 filter=linear
 */

static void view_kv(struct nb_view *v, const char *k, const char *val)
{
    int i;
    unsigned w, h;

    if (!strcmp(k, "scale")) {
        if (nb_view_parse_scale(val, &i) == 0) {
            v->scale = i;
        }
    } else if (!strcmp(k, "filter")) {
        if (nb_view_parse_filter(val, &i) == 0) {
            v->filter = i;
        }
    } else if (!strcmp(k, "area")) {
        struct nb_view t = *v;

        if (nb_view_parse_area(val, &t) == 0) {
            *v = t;
        }
    } else if (!strcmp(k, "custom")) {
        /* The custom area remembered while a preset is in use, so cycling
         * back to Custom after a restart finds it. */
        struct nb_view t = *v;

        if (nb_view_parse_area(val, &t) == 0 && t.area == NB_AREA_CUSTOM) {
            v->cw = t.cw;
            v->ch = t.ch;
            v->cx = t.cx;
            v->cy = t.cy;
            v->ccenter = t.ccenter;
        }
    } else if (!strcmp(k, "res")) {
        if (nb_view_parse_res(val, &w, &h) == 0) {
            v->res_w = w;
            v->res_h = h;
        }
    }
}

/* "scale=stretch area=4:3 ..." -> v */
static void view_parse_words(struct nb_view *v, const char *s)
{
    char tmp[256], *save = NULL, *tok;

    snprintf(tmp, sizeof(tmp), "%s", s);
    for (tok = strtok_r(tmp, " \t", &save); tok;
         tok = strtok_r(NULL, " \t", &save)) {
        char *eq = strchr(tok, '=');

        if (eq) {
            *eq = '\0';
            view_kv(v, tok, eq + 1);
        }
    }
}

static int view_words(const struct nb_view *v, char *buf, size_t n)
{
    char a[40], r[24];

    nb_view_area_str(v, a, sizeof(a));
    nb_view_res_str(v, r, sizeof(r));
    return snprintf(buf, n, "scale=%s filter=%s area=%s res=%s",
                    nb_view_scale_name(v->scale),
                    nb_view_filter_name(v->filter), a, r);
}

void nb_vstore_init(struct nb_vstore *st)
{
    memset(st, 0, sizeof(*st));
    nb_view_defaults(&st->cur);
    st->prof_at = -1;
}

static void clean_name(char *dst, size_t n, const char *src)
{
    size_t i = 0;

    while (*src == ' ') {
        src++;
    }
    for (; *src && i + 1 < n; src++) {
        unsigned char c = (unsigned char)*src;

        if (c < 0x20 || c == '|' || c == 0x7f) {
            continue;
        }
        dst[i++] = (char)c;
    }
    while (i > 0 && dst[i - 1] == ' ') {
        i--;
    }
    dst[i] = '\0';
}

void nb_vstore_parse(struct nb_vstore *st, const char *text)
{
    const char *p = text;

    while (p && *p) {
        const char *nl = strchr(p, '\n');
        size_t len = nl ? (size_t)(nl - p) : strlen(p);
        char line[384], *eq;

        if (len >= sizeof(line)) {
            len = sizeof(line) - 1;
        }
        memcpy(line, p, len);
        line[len] = '\0';
        while (len > 0 && (line[len - 1] == '\r' || line[len - 1] == ' ')) {
            line[--len] = '\0';
        }
        p = nl ? nl + 1 : NULL;
        if (line[0] == '#' || !(eq = strchr(line, '='))) {
            continue;
        }
        *eq = '\0';
        if (!strcmp(line, "recent")) {
            unsigned w, h;

            if (nb_view_parse_res(eq + 1, &w, &h) == 0 && w && h &&
                st->nrecent < NB_MAX_RECENT) {
                st->recent[st->nrecent][0] = w;
                st->recent[st->nrecent][1] = h;
                st->nrecent++;
            }
        } else if (!strcmp(line, "rule")) {
            char *bar = strchr(eq + 1, '|');
            struct nb_rule *ru;
            unsigned w, h;

            if (!bar || st->nrule >= NB_MAX_RULES) {
                continue;
            }
            *bar = '\0';
            if (nb_view_parse_res(eq + 1, &w, &h) != 0 || !w || !h ||
                nb_vstore_rule_find(st, w, h) >= 0) {
                continue;
            }
            ru = &st->rule[st->nrule];
            ru->w = w;
            ru->h = h;
            nb_view_defaults(&ru->v);
            view_parse_words(&ru->v, bar + 1);
            ru->v.res_w = ru->v.res_h = 0;
            st->nrule++;
        } else if (!strcmp(line, "profile")) {
            char *bar = strchr(eq + 1, '|');
            struct nb_profile *pr;

            if (!bar || st->nprof >= NB_MAX_PROFILES) {
                continue;
            }
            *bar = '\0';
            pr = &st->prof[st->nprof];
            clean_name(pr->name, sizeof(pr->name), eq + 1);
            if (!pr->name[0]) {
                continue;
            }
            nb_view_defaults(&pr->v);
            view_parse_words(&pr->v, bar + 1);
            st->nprof++;
        } else {
            view_kv(&st->cur, line, eq + 1);
        }
    }
}

/* cur with the layout fields of `lay` (area, scale, filter); res kept. */
static void take_layout(struct nb_view *dst, const struct nb_view *lay)
{
    unsigned rw = dst->res_w, rh = dst->res_h;

    *dst = *lay;
    dst->res_w = rw;
    dst->res_h = rh;
}

size_t nb_vstore_format(const struct nb_vstore *st0, char *buf, size_t n)
{
    size_t at = 0;
    char a[40], r[24];
    int i;
    /* While a rule is in force the file keeps the layout under it. */
    struct nb_vstore stv = *st0;
    const struct nb_vstore *st = &stv;

    if (stv.ruled) {
        take_layout(&stv.cur, &stv.base);
    }

#define PUT(...) do { \
        int k_ = snprintf(buf + at, at < n ? n - at : 0, __VA_ARGS__); \
        if (k_ > 0) { at += (size_t)k_; } \
    } while (0)
    nb_view_area_str(&st->cur, a, sizeof(a));
    nb_view_res_str(&st->cur, r, sizeof(r));
    PUT("# conduit viewer display settings (Ctrl+Alt+M in the viewer)\n");
    PUT("scale=%s\nfilter=%s\narea=%s\nres=%s\n",
        nb_view_scale_name(st->cur.scale), nb_view_filter_name(st->cur.filter),
        a, r);
    if (st->cur.area != NB_AREA_CUSTOM && st->cur.cw > 0 && st->cur.ch > 0) {
        struct nb_view t = st->cur;

        t.area = NB_AREA_CUSTOM;
        nb_view_area_str(&t, a, sizeof(a));
        PUT("custom=%s\n", a);
    }
    for (i = 0; i < st->nrecent; i++) {
        PUT("recent=%ux%u\n", st->recent[i][0], st->recent[i][1]);
    }
    for (i = 0; i < st->nprof; i++) {
        char words[200];

        view_words(&st->prof[i].v, words, sizeof(words));
        PUT("profile=%s|%s\n", st->prof[i].name, words);
    }
    for (i = 0; i < st->nrule; i++) {
        char words[200];
        struct nb_view t = st->rule[i].v;

        t.res_w = t.res_h = 0;
        view_words(&t, words, sizeof(words));
        PUT("rule=%ux%u|%s\n", st->rule[i].w, st->rule[i].h, words);
    }
#undef PUT
    return at;
}

int nb_vstore_load(struct nb_vstore *st, const char *path)
{
    char buf[8192];
    ssize_t r;
    size_t got = 0;
    int fd;

    if (!path) {
        return 0;
    }
    fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        return errno == ENOENT ? 0 : -1;
    }
    while (got < sizeof(buf) - 1 &&
           (r = read(fd, buf + got, sizeof(buf) - 1 - got)) > 0) {
        got += (size_t)r;
    }
    close(fd);
    buf[got] = '\0';
    nb_vstore_parse(st, buf);
    return 0;
}

int nb_vstore_save(const struct nb_vstore *st, const char *path)
{
    char buf[8192], tmp[4096];
    size_t len, off = 0;
    int fd;

    if (!path) {
        return 0;
    }
    len = nb_vstore_format(st, buf, sizeof(buf));
    if (len >= sizeof(buf)) {
        len = sizeof(buf) - 1;
    }
    if ((size_t)snprintf(tmp, sizeof(tmp), "%s.tmp", path) >= sizeof(tmp)) {
        return -ENAMETOOLONG;
    }
    fd = open(tmp, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0600);
    if (fd < 0) {
        return -errno;
    }
    while (off < len) {
        ssize_t w = write(fd, buf + off, len - off);

        if (w < 0) {
            int e = errno;

            close(fd);
            unlink(tmp);
            return -e;
        }
        off += (size_t)w;
    }
    if (close(fd) != 0 || rename(tmp, path) != 0) {
        int e = errno;

        unlink(tmp);
        return -e;
    }
    return 0;
}

static bool is_preset(unsigned w, unsigned h)
{
    int i;

    for (i = 0; i < NB_RES_PRESETS; i++) {
        if (nb_res_preset[i][0] == w && nb_res_preset[i][1] == h) {
            return true;
        }
    }
    return false;
}

void nb_vstore_note_res(struct nb_vstore *st, unsigned w, unsigned h)
{
    int i, at = st->nrecent;

    if (!w || !h || is_preset(w, h)) {
        return;
    }
    for (i = 0; i < st->nrecent; i++) {
        if (st->recent[i][0] == w && st->recent[i][1] == h) {
            at = i;
            break;
        }
    }
    if (at == st->nrecent && st->nrecent < NB_MAX_RECENT) {
        st->nrecent++;
    }
    if (at >= NB_MAX_RECENT) {
        at = NB_MAX_RECENT - 1;
    }
    memmove(st->recent[1], st->recent[0], sizeof(st->recent[0]) * (size_t)at);
    st->recent[0][0] = w;
    st->recent[0][1] = h;
}

int nb_vstore_profile_save(struct nb_vstore *st, const char *name)
{
    char clean[NB_PROFILE_NAME];
    int i;

    clean_name(clean, sizeof(clean), name ? name : "");
    if (!clean[0]) {
        return -1;
    }
    for (i = 0; i < st->nprof; i++) {
        if (!strcmp(st->prof[i].name, clean)) {
            break;
        }
    }
    if (i == st->nprof) {
        if (st->nprof >= NB_MAX_PROFILES) {
            return -1;
        }
        st->nprof++;
    }
    snprintf(st->prof[i].name, sizeof(st->prof[i].name), "%s", clean);
    st->prof[i].v = st->cur;
    st->prof_at = i;
    return i;
}

int nb_vstore_profile_load(struct nb_vstore *st, int i)
{
    if (i < 0 || i >= st->nprof) {
        return -1;
    }
    st->cur = st->prof[i].v;
    st->prof_at = i;
    return 0;
}

void nb_vstore_profile_delete(struct nb_vstore *st, int i)
{
    if (i < 0 || i >= st->nprof) {
        return;
    }
    memmove(&st->prof[i], &st->prof[i + 1],
            sizeof(st->prof[0]) * (size_t)(st->nprof - i - 1));
    st->nprof--;
    if (st->prof_at == i) {
        st->prof_at = -1;
    } else if (st->prof_at > i) {
        st->prof_at--;
    }
}

/* ── hotkey actions ─────────────────────────────────────────────────────── */

static unsigned view_changes(const struct nb_view *a, const struct nb_view *b)
{
    unsigned ch = 0;

    if (a->scale != b->scale || a->filter != b->filter || a->area != b->area ||
        a->cw != b->cw || a->ch != b->ch || a->cx != b->cx ||
        a->cy != b->cy || a->ccenter != b->ccenter) {
        ch |= NB_VIEW_CH_LAYOUT;
    }
    /* The area decides a native resolution, so it re-hints too. */
    if (a->res_w != b->res_w || a->res_h != b->res_h ||
        ((!b->res_w || !b->res_h) && (ch & NB_VIEW_CH_LAYOUT) &&
         (a->area != b->area || a->cw != b->cw || a->ch != b->ch))) {
        ch |= NB_VIEW_CH_RES;
    }
    return ch;
}

unsigned nb_view_cycle_scale(struct nb_vstore *st, char *label, size_t n)
{
    static const int order[NB_SCALE_COUNT] = {
        NB_SCALE_ASPECT, NB_SCALE_STRETCH, NB_SCALE_INTEGER, NB_SCALE_NONE,
    };
    int i;

    for (i = 0; i < NB_SCALE_COUNT && order[i] != st->cur.scale; i++) {
    }
    st->cur.scale = order[(i + 1) % NB_SCALE_COUNT];
    snprintf(label, n, "Scale: %s", nb_view_scale_label(st->cur.scale));
    return NB_VIEW_CH_LAYOUT;
}

unsigned nb_view_cycle_filter(struct nb_vstore *st, char *label, size_t n)
{
    st->cur.filter = st->cur.filter == NB_FILTER_NEAREST ? NB_FILTER_LINEAR
                                                         : NB_FILTER_NEAREST;
    snprintf(label, n, "Filter: %s",
             st->cur.filter == NB_FILTER_NEAREST ? "Sharp" : "Smooth");
    return NB_VIEW_CH_LAYOUT;
}

unsigned nb_view_cycle_area(struct nb_vstore *st, char *label, size_t n)
{
    struct nb_view old = st->cur;
    int a = st->cur.area + 1;

    if (a == NB_AREA_CUSTOM && (st->cur.cw <= 0 || st->cur.ch <= 0)) {
        a++;                    /* no custom area yet: skip it */
    }
    if (a >= NB_AREA_COUNT) {
        a = NB_AREA_FULL;
    }
    st->cur.area = a;
    if (a == NB_AREA_CUSTOM) {
        char s[40];

        nb_view_area_str(&st->cur, s, sizeof(s));
        snprintf(label, n, "Area: %s", s);
    } else {
        snprintf(label, n, "Area: %s", nb_view_area_label(a));
    }
    return view_changes(&old, &st->cur) | NB_VIEW_CH_LAYOUT;
}

unsigned nb_view_cycle_res(struct nb_vstore *st, char *label, size_t n)
{
    unsigned w = st->cur.res_w, h = st->cur.res_h;
    int i, at = -2;             /* -1 = native, 0.. presets, P = custom */

    if (!w || !h) {
        at = -1;
    } else {
        for (i = 0; i < NB_RES_PRESETS; i++) {
            if (nb_res_preset[i][0] == w && nb_res_preset[i][1] == h) {
                at = i;
            }
        }
        if (at == -2) {
            at = NB_RES_PRESETS;
        }
    }
    at++;
    if (at == NB_RES_PRESETS && st->nrecent == 0) {
        at++;
    }
    if (at > NB_RES_PRESETS) {
        st->cur.res_w = st->cur.res_h = 0;
    } else if (at == NB_RES_PRESETS) {
        st->cur.res_w = st->recent[0][0];
        st->cur.res_h = st->recent[0][1];
    } else {
        st->cur.res_w = nb_res_preset[at][0];
        st->cur.res_h = nb_res_preset[at][1];
    }
    if (st->cur.res_w) {
        snprintf(label, n, "Guest resolution: %ux%u", st->cur.res_w,
                 st->cur.res_h);
    } else {
        snprintf(label, n, "Guest resolution: Native");
    }
    return NB_VIEW_CH_RES;
}

/* ── per-guest-mode rules ────────────────────────────────────────────── */

int nb_vstore_rule_find(const struct nb_vstore *st, unsigned w, unsigned h)
{
    int i;

    for (i = 0; i < st->nrule; i++) {
        if (st->rule[i].w == w && st->rule[i].h == h) {
            return i;
        }
    }
    return -1;
}

int nb_vstore_rule_save(struct nb_vstore *st, unsigned w, unsigned h)
{
    int i = nb_vstore_rule_find(st, w, h);

    if (!w || !h) {
        return -1;
    }
    if (i < 0) {
        if (st->nrule >= NB_MAX_RULES) {
            return -1;
        }
        i = st->nrule++;
    }
    st->rule[i].w = w;
    st->rule[i].h = h;
    st->rule[i].v = st->cur;
    st->rule[i].v.res_w = st->rule[i].v.res_h = 0;
    if (!st->ruled && !st->base_set) {
        st->base = st->cur;
    }
    st->base_set = true;
    st->ruled = true;
    st->rule_w = w;
    st->rule_h = h;
    return i;
}

void nb_vstore_rule_delete(struct nb_vstore *st, int i)
{
    if (i < 0 || i >= st->nrule) {
        return;
    }
    if (st->ruled && st->rule[i].w == st->rule_w &&
        st->rule[i].h == st->rule_h) {
        /* The layout in force stays until the next mode change. */
        st->base = st->cur;
    }
    memmove(&st->rule[i], &st->rule[i + 1],
            (size_t)(st->nrule - i - 1) * sizeof(st->rule[0]));
    st->nrule--;
}

static bool same_layout(const struct nb_view *a, const struct nb_view *b)
{
    return a->scale == b->scale && a->filter == b->filter &&
           a->area == b->area && a->cw == b->cw && a->ch == b->ch &&
           a->cx == b->cx && a->cy == b->cy && a->ccenter == b->ccenter;
}

unsigned nb_vstore_guest_mode(struct nb_vstore *st, unsigned w, unsigned h,
                              char *label, size_t n)
{
    struct nb_view old = st->cur;
    int i = nb_vstore_rule_find(st, w, h);

    if (label && n) {
        label[0] = '\0';
    }
    if (i >= 0) {
        if (st->ruled && st->rule_w == w && st->rule_h == h) {
            return 0;           /* already in force */
        }
        if (!st->ruled) {
            st->base = st->cur;
            st->base_set = true;
        }
        take_layout(&st->cur, &st->rule[i].v);
        st->ruled = true;
        st->rule_w = w;
        st->rule_h = h;
        if (label && n) {
            char a[40];

            nb_view_area_str(&st->cur, a, sizeof(a));
            snprintf(label, n, "%ux%u: %s, %s", w, h,
                     st->cur.area == NB_AREA_FULL ? "full" : a,
                     nb_view_scale_name(st->cur.scale));
        }
    } else if (st->ruled) {
        take_layout(&st->cur, &st->base);
        st->ruled = false;
        if (label && n) {
            snprintf(label, n, "%ux%u: usual picture settings", w, h);
        }
    } else {
        /* A mode without a rule: what the picture looks like on arrival
         * is what a rule remembered here later comes back to. */
        st->base = st->cur;
        st->base_set = true;
        return 0;
    }
    return same_layout(&old, &st->cur) ? 0 : NB_VIEW_CH_LAYOUT;
}

/* ── the display's mode list ─────────────────────────────────────────── */

int nb_modes_feed(struct nb_modes_rx *rx, unsigned w, unsigned h, unsigned idx,
                  unsigned count, unsigned mhz, unsigned long long gen,
                  unsigned flags)
{
    struct nb_modes *in = &rx->in;

    if (!count || count > NB_MODES_MAX || idx >= count || !w || !h ||
        w > 8192 || h > 8192 || (idx == 0) != !!(flags & NB_MODE_NATIVE) ||
        idx != rx->next ||
        (idx > 0 && (in->gen != gen || in->n != count || in->mhz != mhz))) {
        rx->next = 0;
        /* A record 0 that broke the order starts afresh next time. */
        return -1;
    }
    if (idx == 0) {
        memset(in, 0, sizeof(*in));
        in->n = count;
        in->gen = gen;
        in->mhz = mhz;
    }
    in->w[idx] = w;
    in->h[idx] = h;
    in->flags[idx] = flags & (NB_MODE_NATIVE | NB_MODE_CUSTOM);
    rx->next = idx + 1;
    if (rx->next < count) {
        return 0;
    }
    rx->cur = *in;
    rx->next = 0;
    return 1;
}

int nb_modes_find(const struct nb_modes *m, unsigned w, unsigned h)
{
    unsigned i;

    for (i = 0; m && i < m->n; i++) {
        if (m->w[i] == w && m->h[i] == h) {
            return (int)i;
        }
    }
    return -1;
}

unsigned nb_view_cycle_mode(struct nb_vstore *st, const struct nb_modes *m,
                            unsigned cur_w, unsigned cur_h, char *label,
                            size_t n)
{
    int at;

    if (!m || !m->n) {
        return nb_view_cycle_res(st, label, n);
    }
    at = nb_modes_find(m, cur_w, cur_h);
    at = at < 0 ? 0 : (at + 1) % (int)m->n;
    st->cur.res_w = m->w[at];
    st->cur.res_h = m->h[at];
    snprintf(label, n, "Guest resolution: %ux%u%s", m->w[at], m->h[at],
             (m->flags[at] & NB_MODE_NATIVE) ? " (native)" : "");
    return NB_VIEW_CH_RES;
}

unsigned nb_view_cycle_profile(struct nb_vstore *st, char *label, size_t n)
{
    struct nb_view old = st->cur;
    int i;

    if (st->nprof == 0) {
        snprintf(label, n, "No profiles saved (Ctrl+Alt+M)");
        return 0;
    }
    i = st->prof_at + 1 >= st->nprof ? 0 : st->prof_at + 1;
    nb_vstore_profile_load(st, i);
    snprintf(label, n, "Profile: %s", st->prof[i].name);
    return view_changes(&old, &st->cur) | NB_VIEW_CH_LAYOUT;
}

unsigned nb_view_nudge(struct nb_vstore *st, int ww, int wh, unsigned s120,
                       int dx, int dy, int dw, char *label, size_t n)
{
    struct nb_view old = st->cur;
    struct nb_rect a;
    int W = to_px(ww > 0 ? ww : 1, s120), H = to_px(wh > 0 ? wh : 1, s120);

    nb_view_area_px(&st->cur, ww, wh, s120, &a);
    if (st->cur.area == NB_AREA_FULL) {
        a.x = a.y = 0;
        a.w = W;
        a.h = H;
    }
    if (dw) {
        int cxm = a.x + a.w / 2, cym = a.y + a.h / 2;
        int nw = a.w + dw, nh;

        if (nw < 64) { nw = 64; }
        if (nw > W) { nw = W; }
        nh = (int)((long)nw * a.h / (a.w > 0 ? a.w : 1));
        if (nh > H) {
            nh = H;
            nw = (int)((long)nh * a.w / (a.h > 0 ? a.h : 1));
        }
        if (nh < 36) { nh = 36; }
        a.x = cxm - nw / 2;
        a.y = cym - nh / 2;
        a.w = nw;
        a.h = nh;
    }
    a.x += dx;
    a.y += dy;
    if (a.x < 0) { a.x = 0; }
    if (a.y < 0) { a.y = 0; }
    if (a.x + a.w > W) { a.x = W - a.w; }
    if (a.y + a.h > H) { a.y = H - a.h; }
    st->cur.area = NB_AREA_CUSTOM;
    st->cur.cw = a.w;
    st->cur.ch = a.h;
    st->cur.cx = a.x;
    st->cur.cy = a.y;
    /* Still centred if it is (a resize about the centre keeps it so). */
    st->cur.ccenter = (old.area != NB_AREA_CUSTOM || old.ccenter) && !dx &&
                      !dy && a.x == (W - a.w) / 2 && a.y == (H - a.h) / 2;
    {
        char s[40];

        nb_view_area_str(&st->cur, s, sizeof(s));
        snprintf(label, n, "Area: %s", s);
    }
    return view_changes(&old, &st->cur) | NB_VIEW_CH_LAYOUT;
}

unsigned nb_view_reset(struct nb_vstore *st, char *label, size_t n)
{
    struct nb_view old = st->cur;

    nb_view_defaults(&st->cur);
    st->prof_at = -1;
    snprintf(label, n, "Display settings reset");
    return view_changes(&old, &st->cur) | NB_VIEW_CH_LAYOUT;
}

void nb_view_override(struct nb_view *dst, const struct nb_view *src,
                      unsigned mask)
{
    if (mask & NB_VIEW_SET_SCALE) {
        dst->scale = src->scale;
    }
    if (mask & NB_VIEW_SET_FILTER) {
        dst->filter = src->filter;
    }
    if (mask & NB_VIEW_SET_AREA) {
        dst->area = src->area;
        dst->cw = src->cw;
        dst->ch = src->ch;
        dst->cx = src->cx;
        dst->cy = src->cy;
        dst->ccenter = src->ccenter;
    }
    if (mask & NB_VIEW_SET_RES) {
        dst->res_w = src->res_w;
        dst->res_h = src->res_h;
    }
}
