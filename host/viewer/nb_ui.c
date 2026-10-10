/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * nb_ui.c -- the settings menu, the on-screen notice and the top-edge button,
 * painted into plain ARGB memory.  See nb_ui.h.
 *
 * ONE CODE PATH FOR LAYOUT AND PAINT.  build() walks the panel top to bottom;
 * without a painter it only records the clickable items, with one it also
 * draws them.  Hit-testing therefore uses exactly the rectangles that were
 * drawn, and the two cannot drift apart.
 */
#include "nb_ui.h"

#include <ctype.h>
#include <linux/input-event-codes.h>
#include <math.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "nvkvm_broker.h"

#ifdef NB_HAVE_FREETYPE
#include <ft2build.h>
#include FT_FREETYPE_H
#include <fontconfig/fontconfig.h>
#endif

/* ── look ───────────────────────────────────────────────────────────────── */

#define UI_W        360         /* panel width, logical                      */
#define UI_PAD      16
#define UI_RADIUS   12
#define UI_CHIP_H   28
#define UI_CHIP_PADX 12
#define UI_GAP      8
#define UI_FIELD_H  30
#define UI_SEC_GAP  14

#define F_WORD      18          /* wordmark                                  */
#define F_BODY      13          /* chips, fields                             */
#define F_SMALL     12          /* summary, hints                            */
#define F_SEC       11          /* section headers                           */
#define F_NOTICE    15
#define F_BUTTON    12

/* Non-premultiplied ARGB. */
#define C_BG        0xe60d1117u
#define C_BG_OPAQUE 0xff0d1117u
#define C_EDGE      0x22ffffffu
#define C_TEXT      0xffe6edf3u
#define C_DIM       0xff8b949eu
#define C_FAINT     0xff484f58u
#define C_GREEN     0xff3fb950u
#define C_GREEN_HI  0xff56d364u
#define C_ON_GREEN  0xff0d1117u
#define C_CHIP      0x14ffffffu
#define C_CHIP_HOT  0x2effffffu
#define C_CHIP_EDGE 0x30ffffffu
#define C_FIELD     0xff161b22u
#define C_RED       0xfff85149u
#define C_RED_DIM   0x40f85149u

/* ── fonts ──────────────────────────────────────────────────────────────── */

static bool force_bitmap;

void nb_ui_force_bitmap_font(bool on)
{
    force_bitmap = on;
}

#ifdef NB_HAVE_FREETYPE
struct glyph {
    uint8_t *bm;
    int      w, h, pitch, left, top, adv;
    bool     done;
};
struct gcache {
    int          face, size;    /* size 0 = slot free                        */
    struct glyph g[95];
};
#define NB_GCACHE 12
static struct gcache gcache[NB_GCACHE];
static int gcache_next;
static FT_Library ft_lib;
static FT_Face ft_face[2];      /* 0 regular, 1 bold                         */
static int ft_state;            /* 0 untried, 1 ready, -1 unavailable        */

static FT_Face ft_open(const char *pattern)
{
    FcPattern *p, *m;
    FcResult r;
    FcChar8 *file = NULL;
    FT_Face f = NULL;

    p = FcNameParse((const FcChar8 *)pattern);
    if (!p) {
        return NULL;
    }
    FcConfigSubstitute(NULL, p, FcMatchPattern);
    FcDefaultSubstitute(p);
    m = FcFontMatch(NULL, p, &r);
    FcPatternDestroy(p);
    if (!m) {
        return NULL;
    }
    if (FcPatternGetString(m, FC_FILE, 0, &file) == FcResultMatch && file) {
        if (FT_New_Face(ft_lib, (const char *)file, 0, &f) != 0) {
            f = NULL;
        }
    }
    FcPatternDestroy(m);
    return f;
}

static bool ft_ready(void)
{
    if (force_bitmap) {
        return false;
    }
    if (ft_state == 0) {
        ft_state = -1;
        if (FT_Init_FreeType(&ft_lib) == 0 && FcInit()) {
            ft_face[0] = ft_open("sans:weight=medium");
            ft_face[1] = ft_open("sans:bold");
            if (!ft_face[0]) {
                ft_face[0] = ft_face[1];
            }
            if (!ft_face[1]) {
                ft_face[1] = ft_face[0];
            }
            if (ft_face[0]) {
                ft_state = 1;
            }
        }
    }
    return ft_state == 1;
}

static struct gcache *gc_get(int face, int size)
{
    struct gcache *c;
    int i;

    for (i = 0; i < NB_GCACHE; i++) {
        if (gcache[i].size == size && gcache[i].face == face) {
            return &gcache[i];
        }
    }
    c = &gcache[gcache_next];
    gcache_next = (gcache_next + 1) % NB_GCACHE;
    for (i = 0; i < 95; i++) {
        free(c->g[i].bm);
    }
    memset(c, 0, sizeof(*c));
    c->face = face;
    c->size = size;
    return c;
}

static const struct glyph *ft_glyph(int face, int size, char ch)
{
    struct gcache *c;
    struct glyph *g;
    FT_Face f = ft_face[face];
    FT_GlyphSlot sl;
    int idx = (unsigned char)ch - 32;

    if (idx < 0 || idx >= 95) {
        idx = '?' - 32;
    }
    c = gc_get(face, size);
    g = &c->g[idx];
    if (g->done) {
        return g;
    }
    g->done = true;
    if (FT_Set_Pixel_Sizes(f, 0, (FT_UInt)size) != 0 ||
        FT_Load_Char(f, (FT_ULong)(idx + 32),
                     FT_LOAD_RENDER | FT_LOAD_TARGET_LIGHT) != 0) {
        return g;
    }
    sl = f->glyph;
    g->adv = (int)((sl->advance.x + 32) >> 6);
    g->left = sl->bitmap_left;
    g->top = sl->bitmap_top;
    if (sl->bitmap.pixel_mode == FT_PIXEL_MODE_GRAY && sl->bitmap.rows &&
        sl->bitmap.width) {
        int pitch = sl->bitmap.pitch < 0 ? -sl->bitmap.pitch : sl->bitmap.pitch;

        g->bm = malloc((size_t)pitch * sl->bitmap.rows);
        if (g->bm) {
            memcpy(g->bm, sl->bitmap.buffer, (size_t)pitch * sl->bitmap.rows);
            g->w = (int)sl->bitmap.width;
            g->h = (int)sl->bitmap.rows;
            g->pitch = pitch;
        }
    }
    return g;
}
#endif

/* The bitmap font's integer scale for a pixel size. */
static unsigned bm_scale(int size)
{
    int s = (size + 3) / 7;

    return s < 1 ? 1u : (unsigned)s;
}

static void upper(char *dst, size_t n, const char *s)
{
    size_t i;

    for (i = 0; s[i] && i + 1 < n; i++) {
        dst[i] = (char)toupper((unsigned char)s[i]);
    }
    dst[i] = '\0';
}

/* Width in pixels of s at pixel size `size`, plus `sp` px between letters. */
static int text_w(const char *s, int size, bool bold, int sp)
{
    int n = (int)strlen(s);

    if (!n) {
        return 0;
    }
#ifdef NB_HAVE_FREETYPE
    if (ft_ready()) {
        int w = 0;

        for (; *s; s++) {
            w += ft_glyph(bold, size, *s)->adv;
        }
        return w + sp * (n - 1);
    }
#endif
    {
        char u[256];

        (void)bold;
        upper(u, sizeof(u), s);
        return (int)nb_placeholder_text_w(u, bm_scale(size)) + sp * (n - 1);
    }
}

/* ── painter ────────────────────────────────────────────────────────────── */

struct pnt {
    uint32_t *px;
    int       w, h, stride;     /* physical                                  */
    double    k;                /* physical px per logical px                */
    double    oy;               /* logical y shown at the buffer's top       */
    double    alpha;            /* global multiplier                         */
};

static inline void blend(struct pnt *p, int x, int y, uint32_t c, double cov)
{
    uint32_t *d;
    unsigned sa, a, r, g, b, inv;
    uint32_t dv;

    if (x < 0 || y < 0 || x >= p->w || y >= p->h || cov <= 0) {
        return;
    }
    if (cov > 1) {
        cov = 1;
    }
    sa = (unsigned)((c >> 24) * cov * p->alpha + 0.5);
    if (!sa) {
        return;
    }
    d = &p->px[(size_t)y * p->stride + x];
    dv = *d;
    inv = 255 - sa;
    a = sa + ((dv >> 24) * inv + 127) / 255;
    r = (((c >> 16) & 0xff) * sa + 127) / 255 + (((dv >> 16) & 0xff) * inv + 127) / 255;
    g = (((c >> 8) & 0xff) * sa + 127) / 255 + (((dv >> 8) & 0xff) * inv + 127) / 255;
    b = ((c & 0xff) * sa + 127) / 255 + ((dv & 0xff) * inv + 127) / 255;
    if (a > 255) { a = 255; }
    if (r > a) { r = a; }
    if (g > a) { g = a; }
    if (b > a) { b = a; }
    *d = (a << 24) | (r << 16) | (g << 8) | b;
}

/* Signed distance from (x, y) to a rounded box (physical). */
static double rbox_sd(double x, double y, double x0, double y0, double x1,
                      double y1, double r)
{
    double cx = (x0 + x1) / 2, cy = (y0 + y1) / 2;
    double hx = (x1 - x0) / 2 - r, hy = (y1 - y0) / 2 - r;
    double qx = fabs(x - cx) - hx, qy = fabs(y - cy) - hy;
    double ox = qx > 0 ? qx : 0, oy = qy > 0 ? qy : 0;
    double in = qx > qy ? qx : qy;

    return sqrt(ox * ox + oy * oy) + (in < 0 ? in : 0) - r;
}

/* Fill (t <= 0) or stroke (t = width, physical) a rounded rectangle given in
 * LOGICAL coordinates. */
static void p_rrect(struct pnt *p, double lx, double ly, double lw, double lh,
                    double lr, uint32_t c, double t)
{
    double x0 = lx * p->k, y0 = (ly - p->oy) * p->k;
    double x1 = (lx + lw) * p->k, y1 = (ly + lh - p->oy) * p->k;
    double r = lr * p->k;
    int ix0 = (int)floor(x0), iy0 = (int)floor(y0);
    int ix1 = (int)ceil(x1), iy1 = (int)ceil(y1);
    int x, y;

    if (r > (x1 - x0) / 2) { r = (x1 - x0) / 2; }
    if (r > (y1 - y0) / 2) { r = (y1 - y0) / 2; }
    if (ix0 < 0) { ix0 = 0; }
    if (iy0 < 0) { iy0 = 0; }
    if (ix1 > p->w) { ix1 = p->w; }
    if (iy1 > p->h) { iy1 = p->h; }
    for (y = iy0; y < iy1; y++) {
        for (x = ix0; x < ix1; x++) {
            double d = rbox_sd(x + 0.5, y + 0.5, x0, y0, x1, y1, r);
            double cov;

            if (t > 0) {
                cov = 0.5 - fabs(d + t / 2) + t / 2;
            } else {
                cov = 0.5 - d;
            }
            if (cov > 0) {
                blend(p, x, y, c, cov);
            }
        }
    }
}

static void p_circle(struct pnt *p, double lcx, double lcy, double lr,
                     uint32_t c)
{
    p_rrect(p, lcx - lr, lcy - lr, 2 * lr, 2 * lr, lr, c, 0);
}

/*
 * Text with its vertical centre at logical ly, starting at logical lx (align
 * 0), centred on lx (1) or ending at lx (2).
 */
static void p_text(struct pnt *p, double lx, double ly, const char *s,
                   int lsize, bool bold, uint32_t c, int align, double lsp)
{
    int size = (int)(lsize * p->k + 0.5);
    int sp = (int)(lsp * p->k + 0.5);
    int w, x, base;
    double cy = (ly - p->oy) * p->k;

    if (size < 6) {
        size = 6;
    }
    w = text_w(s, size, bold, sp);
    x = (int)(lx * p->k + 0.5);
    if (align == 1) {
        x -= w / 2;
    } else if (align == 2) {
        x -= w;
    }
#ifdef NB_HAVE_FREETYPE
    if (ft_ready()) {
        base = (int)(cy + size * 0.36 + 0.5);
        for (; *s; s++) {
            const struct glyph *g = ft_glyph(bold, size, *s);
            int gx, gy;

            for (gy = 0; g->bm && gy < g->h; gy++) {
                for (gx = 0; gx < g->w; gx++) {
                    uint8_t v = g->bm[(size_t)gy * g->pitch + gx];

                    if (v) {
                        blend(p, x + g->left + gx, base - g->top + gy, c,
                              v / 255.0);
                    }
                }
            }
            x += g->adv + sp;
        }
        return;
    }
#endif
    {
        unsigned bs = bm_scale(size);
        int th = 7 * (int)bs, tw;
        char u[256];
        uint32_t *tmp;
        int i, j;

        upper(u, sizeof(u), s);
        tw = (int)nb_placeholder_text_w(u, bs);
        if (tw <= 0) {
            return;
        }
        tmp = calloc((size_t)tw * th, sizeof(*tmp));
        if (!tmp) {
            return;
        }
        (void)base;
        nb_placeholder_text(tmp, (unsigned)tw, (unsigned)th, (unsigned)tw, 0,
                            0, u, bs, 0xffffffffu);
        base = (int)(cy - th / 2.0 + 0.5);
        for (j = 0; j < th; j++) {
            for (i = 0; i < tw; i++) {
                if (tmp[(size_t)j * tw + i]) {
                    blend(p, x + i, base + j, c, 1.0);
                }
            }
        }
        free(tmp);
    }
}

/* ── layout + paint ─────────────────────────────────────────────────────── */

struct bld {
    struct nb_ui            *ui;
    const struct nb_ui_env  *env;
    struct pnt              *p;     /* NULL: layout only                     */
    int                      y;     /* content cursor, logical               */
    int                      x;     /* chip flow cursor                      */
    int                      lw;
};

static int add_item(struct bld *b, int x, int y, int w, int h, int id, int arg,
                    bool disabled)
{
    struct nb_ui *ui = b->ui;
    struct nb_ui_item *it;

    if (ui->nitem >= NB_UI_MAX_ITEMS) {
        return -1;
    }
    it = &ui->item[ui->nitem];
    it->r.x = x;
    it->r.y = y;
    it->r.w = w;
    it->r.h = h;
    it->id = id;
    it->arg = arg;
    it->disabled = disabled;
    return ui->nitem++;
}

static void flow_break(struct bld *b);

static void section(struct bld *b, const char *title)
{
    flow_break(b);              /* close the previous row of chips first */
    b->y += UI_SEC_GAP;
    if (b->p) {
        char u[64];

        upper(u, sizeof(u), title);
        p_text(b->p, UI_PAD, b->y + 7, u, F_SEC, true, C_DIM, 0, 1.2);
    }
    b->y += 14 + UI_GAP;
    b->x = UI_PAD;
}

static void flow_break(struct bld *b)
{
    if (b->x > UI_PAD) {
        b->y += UI_CHIP_H + UI_GAP;
        b->x = UI_PAD;
    }
}

static void chip_paint(struct bld *b, int idx, const char *label, bool on,
                       uint32_t accent)
{
    struct nb_ui *ui = b->ui;
    struct nb_ui_item *it = &ui->item[idx];
    bool hot = ui->hover == idx && !it->disabled;
    bool down = hot && ui->press == idx;
    uint32_t bg, fg;

    if (!b->p) {
        return;
    }
    if (on) {
        bg = hot ? (accent == C_GREEN ? C_GREEN_HI : accent) : accent;
        fg = C_ON_GREEN;
    } else {
        bg = down ? 0x40ffffffu : hot ? C_CHIP_HOT : C_CHIP;
        fg = it->disabled ? C_FAINT : C_TEXT;
    }
    p_rrect(b->p, it->r.x, it->r.y, it->r.w, it->r.h, it->r.h / 2.0, bg, 0);
    if (!on) {
        p_rrect(b->p, it->r.x, it->r.y, it->r.w, it->r.h, it->r.h / 2.0,
                hot ? 0x60ffffffu : C_CHIP_EDGE, b->p->k);
    }
    p_text(b->p, it->r.x + it->r.w / 2.0, it->r.y + it->r.h / 2.0, label,
           F_BODY, on, fg, 1, 0);
}

/* A pill in the flow; wraps to the next row when it does not fit. */
static int chip(struct bld *b, const char *label, int id, int arg, bool on,
                bool disabled)
{
    int w = text_w(label, F_BODY, true, 0) + 2 * UI_CHIP_PADX;
    int idx;

    if (w > b->lw - 2 * UI_PAD) {
        w = b->lw - 2 * UI_PAD;
    }
    if (b->x > UI_PAD && b->x + w > b->lw - UI_PAD) {
        flow_break(b);
    }
    idx = add_item(b, b->x, b->y, w, UI_CHIP_H, id, arg, disabled);
    b->x += w + UI_GAP;
    if (idx >= 0) {
        chip_paint(b, idx, label, on, C_GREEN);
    }
    return idx;
}

/* A full-width text field. */
static void field(struct bld *b, int id, const char *placeholder)
{
    struct nb_ui *ui = b->ui;
    int idx;
    bool focus = ui->focus == id;

    flow_break(b);
    idx = add_item(b, UI_PAD, b->y, b->lw - 2 * UI_PAD, UI_FIELD_H, id, 0,
                   false);
    b->y += UI_FIELD_H + UI_GAP;
    if (idx < 0 || !b->p) {
        return;
    }
    {
        struct nb_ui_item *it = &ui->item[idx];
        bool hot = ui->hover == idx;
        double cy = it->r.y + it->r.h / 2.0;

        p_rrect(b->p, it->r.x, it->r.y, it->r.w, it->r.h, 8, C_FIELD, 0);
        p_rrect(b->p, it->r.x, it->r.y, it->r.w, it->r.h, 8,
                focus ? C_GREEN : hot ? 0x60ffffffu : C_CHIP_EDGE,
                b->p->k * (focus ? 1.5 : 1.0));
        if (focus) {
            /* measured at the logical size, so it is a logical offset */
            double tl = text_w(ui->text, F_BODY, false, 0);

            p_text(b->p, it->r.x + 10, cy, ui->text, F_BODY, false, C_TEXT, 0,
                   0);
            /* the caret: text measured at logical size */
            p_rrect(b->p, it->r.x + 10 + tl + 1, cy - 8, 1.5, 16, 0, C_GREEN,
                    0);
        } else {
            p_text(b->p, it->r.x + 10, cy, placeholder, F_BODY, false, C_DIM,
                   0, 0);
        }
    }
}

static void hint(struct bld *b, const char *s)
{
    flow_break(b);
    if (b->p) {
        p_text(b->p, UI_PAD, b->y + 8, s, F_SMALL, false, C_DIM, 0, 0);
    }
    b->y += 16 + UI_GAP / 2;
}

static void build(struct nb_ui *ui, const struct nb_ui_env *env, struct pnt *p)
{
    const struct nb_vstore *st = ui->st;
    const struct nb_view *v = &st->cur;
    struct bld b = { .ui = ui, .env = env, .p = p, .y = UI_PAD,
                     .x = UI_PAD, .lw = ui->lw };
    char s[96];
    int i, idx;

    ui->nitem = 0;

    /* header: wordmark, close, summary */
    if (p) {
        p_circle(p, UI_PAD + 5, b.y + 11, 5, C_GREEN);
        p_text(p, UI_PAD + 16, b.y + 11, "CONDUIT", F_WORD, true, C_GREEN, 0,
               3.0);
    }
    idx = add_item(&b, ui->lw - UI_PAD - 24, b.y - 1, 24, 24, NB_UI_ID_CLOSE,
                   0, false);
    if (p && idx >= 0) {
        struct nb_ui_item *it = &ui->item[idx];
        double cx = it->r.x + 12, cy = it->r.y + 12;

        if (ui->hover == idx) {
            p_circle(p, cx, cy, 12, C_CHIP_HOT);
        }
        /* an x from two thin rotated bars, as dots along the diagonals */
        for (i = -5; i <= 5; i++) {
            p_circle(p, cx + i * 0.9, cy + i * 0.9, 1.1, C_TEXT);
            p_circle(p, cx + i * 0.9, cy - i * 0.9, 1.1, C_TEXT);
        }
    }
    b.y += 26;
    nb_view_summary(v, s, sizeof(s));
    if (p) {
        p_text(p, UI_PAD, b.y + 8, s, F_SMALL, false, C_DIM, 0, 0);
    }
    b.y += 16;

    /* resolution */
    section(&b, "Guest resolution");
    chip(&b, "Native", NB_UI_ID_RES_NATIVE, 0, !v->res_w || !v->res_h, false);
    for (i = 0; i < NB_RES_PRESETS; i++) {
        snprintf(s, sizeof(s), "%ux%u", nb_res_preset[i][0],
                 nb_res_preset[i][1]);
        chip(&b, s, NB_UI_ID_RES_PRESET, i,
             v->res_w == nb_res_preset[i][0] && v->res_h == nb_res_preset[i][1],
             false);
    }
    for (i = 0; i < st->nrecent; i++) {
        snprintf(s, sizeof(s), "%ux%u", st->recent[i][0], st->recent[i][1]);
        chip(&b, s, NB_UI_ID_RES_RECENT, i,
             v->res_w == st->recent[i][0] && v->res_h == st->recent[i][1],
             false);
    }
    field(&b, NB_UI_ID_RES_FIELD, "Custom WxH, Enter to apply");

    /* area */
    section(&b, "Picture area");
    for (i = NB_AREA_FULL; i < NB_AREA_CUSTOM; i++) {
        chip(&b, i == NB_AREA_FULL ? "Full" : nb_view_area_label(i),
             NB_UI_ID_AREA, i, v->area == i, false);
    }
    if (v->cw > 0 && v->ch > 0) {
        if (v->area == NB_AREA_CUSTOM) {
            nb_view_area_str(v, s, sizeof(s));
        } else {
            snprintf(s, sizeof(s), "Custom");
        }
        chip(&b, s, NB_UI_ID_AREA, NB_AREA_CUSTOM, v->area == NB_AREA_CUSTOM,
             false);
    }
    chip(&b, "Reset area", NB_UI_ID_AREA_RESET, 0, false, false);
    hint(&b, "Drag the picture's edges or body to adjust");

    /* scaling */
    section(&b, "Scaling");
    {
        static const int order[NB_SCALE_COUNT] = {
            NB_SCALE_ASPECT, NB_SCALE_STRETCH, NB_SCALE_INTEGER, NB_SCALE_NONE,
        };

        for (i = 0; i < NB_SCALE_COUNT; i++) {
            chip(&b, nb_view_scale_label(order[i]), NB_UI_ID_SCALE, order[i],
                 v->scale == order[i], false);
        }
    }

    section(&b, "Filter");
    chip(&b, "Smooth", NB_UI_ID_FILTER, NB_FILTER_LINEAR,
         v->filter == NB_FILTER_LINEAR || !env->nearest_ok, false);
    chip(&b, env->nearest_ok ? "Sharp" : "Sharp (not on this display)",
         NB_UI_ID_FILTER, NB_FILTER_NEAREST,
         env->nearest_ok && v->filter == NB_FILTER_NEAREST, !env->nearest_ok);

    /* profiles */
    section(&b, "Profiles");
    for (i = 0; i < st->nprof; i++) {
        int row = add_item(&b, UI_PAD, b.y, b.lw - 2 * UI_PAD - 32, UI_FIELD_H,
                           NB_UI_ID_PROF_LOAD, i, false);
        int del = add_item(&b, b.lw - UI_PAD - 28, b.y + 1, 28, 28,
                           NB_UI_ID_PROF_DEL, i, false);
        bool armed = ui->confirm == ((NB_UI_ID_PROF_DEL << 8) | (i & 0xff));

        if (p && row >= 0 && del >= 0) {
            struct nb_ui_item *r = &ui->item[row], *d = &ui->item[del];
            bool cur = st->prof_at == i;
            double cy = r->r.y + r->r.h / 2.0;

            p_rrect(p, r->r.x, r->r.y, r->r.w, r->r.h, 8,
                    ui->hover == row ? C_CHIP_HOT : C_CHIP, 0);
            if (cur) {
                p_rrect(p, r->r.x, r->r.y, 3, r->r.h, 1.5, C_GREEN, 0);
            }
            p_text(p, r->r.x + 12, cy,
                   armed ? "Click x again to delete" : st->prof[i].name,
                   F_BODY, cur, armed ? C_RED : C_TEXT, 0, 0);
            p_rrect(p, d->r.x, d->r.y, d->r.w, d->r.h, 14,
                    armed ? C_RED_DIM : ui->hover == del ? C_CHIP_HOT : 0, 0);
            p_text(p, d->r.x + 14, d->r.y + 14, "x", F_BODY, true,
                   armed ? C_RED : C_DIM, 1, 0);
        }
        b.y += UI_FIELD_H + 6;
    }
    field(&b, NB_UI_ID_PROF_FIELD, "Save current as... (name, Enter)");

    /* view */
    section(&b, "View");
    if (!env->no_stats) {
        chip(&b, "Stats overlay", NB_UI_ID_STATS, 0, env->stats_on, false);
    }
    chip(&b, "Fullscreen", NB_UI_ID_FULLSCREEN, 0, env->fullscreen, false);
    flow_break(&b);

    /* VM */
    if (env->vm_actions) {
        int half = (b.lw - 2 * UI_PAD - UI_GAP) / 2;
        static const int ids[2] = { NB_UI_ID_VM_REBOOT, NB_UI_ID_VM_SHUTDOWN };
        static const char *const names[2] = { "Restart", "Shut down" };

        section(&b, "Virtual machine");
        for (i = 0; i < 2; i++) {
            bool armed = ui->confirm == (ids[i] << 8);

            idx = add_item(&b, UI_PAD + i * (half + UI_GAP), b.y, half,
                           UI_CHIP_H, ids[i], 0, false);
            if (idx >= 0 && p) {
                if (armed) {
                    chip_paint(&b, idx, "Click to confirm", true, C_RED);
                } else {
                    chip_paint(&b, idx, names[i], false, C_GREEN);
                }
            }
        }
        b.y += UI_CHIP_H + UI_GAP;
    }

    /* footer */
    b.y += 6;
    if (p) {
        p_text(p, b.lw / 2.0, b.y + 8, "Ctrl+Alt+M or Esc to close",
               F_SMALL, false, C_FAINT, 1, 0);
    }
    b.y += 16 + UI_PAD;
    ui->content_h = b.y;
}

/* ── public: lifecycle, layout ──────────────────────────────────────────── */

void nb_ui_init(struct nb_ui *ui, struct nb_vstore *st)
{
    memset(ui, 0, sizeof(*ui));
    ui->st = st;
    ui->hover = ui->press = -1;
    ui->drag = -1;
    ui->hot = NB_UI_CUR_ARROW;
    ui->lw = UI_W;
}

void nb_ui_layout(struct nb_ui *ui, const struct nb_ui_env *env, int *lw,
                  int *lh)
{
    int maxh = env->win_h - 32;

    ui->lw = UI_W;
    if (env->win_w > 0 && env->win_w < UI_W + 32) {
        ui->lw = env->win_w - 16 > 220 ? env->win_w - 16 : 220;
    }
    build(ui, env, NULL);
    if (maxh < 120) {
        maxh = 120;
    }
    ui->lh = ui->content_h < maxh ? ui->content_h : maxh;
    if (ui->scroll > ui->content_h - ui->lh) {
        ui->scroll = ui->content_h - ui->lh;
    }
    if (ui->scroll < 0) {
        ui->scroll = 0;
    }
    if (ui->hover >= ui->nitem) {
        ui->hover = -1;
    }
    if (ui->press >= ui->nitem) {
        ui->press = -1;
    }
    if (lw) {
        *lw = ui->lw;
    }
    if (lh) {
        *lh = ui->lh;
    }
}

void nb_ui_panel_rect(const struct nb_ui *ui, const struct nb_ui_env *env,
                      struct nb_rect *r)
{
    r->w = ui->lw;
    r->h = ui->lh;
    if (env->win_w >= ui->lw + 32) {
        r->x = env->win_w - 16 - ui->lw;
    } else {
        r->x = (env->win_w - ui->lw) / 2;
    }
    r->y = (env->win_h - ui->lh) / 2;
    if (r->y < 0) {
        r->y = 0;
    }
}

int nb_ui_find(const struct nb_ui *ui, int id, int arg)
{
    int i;

    for (i = 0; i < ui->nitem; i++) {
        if (ui->item[i].id == id && ui->item[i].arg == arg) {
            return i;
        }
    }
    return -1;
}

unsigned nb_ui_set_open(struct nb_ui *ui, const struct nb_ui_env *env,
                        bool open)
{
    bool was = ui->open;

    ui->open = open;
    ui->hover = ui->press = -1;
    ui->confirm = 0;
    ui->focus = 0;
    ui->text[0] = '\0';
    ui->drag = -1;
    ui->hot = NB_UI_CUR_ARROW;
    if (open) {
        nb_ui_layout(ui, env, NULL, NULL);
        return NB_UI_REDRAW;
    }
    return was ? (NB_UI_CLOSE | NB_UI_REDRAW) : 0;
}

/* ── actions ────────────────────────────────────────────────────────────── */

static bool same_area(const struct nb_view *a, const struct nb_view *b)
{
    return a->area == b->area && a->cw == b->cw && a->ch == b->ch &&
           a->cx == b->cx && a->cy == b->cy && a->ccenter == b->ccenter;
}

/* What changed between two views, as NB_UI_* bits (0 = nothing). */
static unsigned diff(const struct nb_view *o, const struct nb_view *n)
{
    unsigned bits = 0;
    bool native = !n->res_w || !n->res_h;

    if (o->scale != n->scale || o->filter != n->filter || !same_area(o, n)) {
        bits |= NB_UI_VIEW;
    }
    if (o->res_w != n->res_w || o->res_h != n->res_h ||
        (native && (o->area != n->area || o->cw != n->cw || o->ch != n->ch))) {
        bits |= NB_UI_RES;
    }
    if (bits) {
        bits |= NB_UI_SAVE | NB_UI_REDRAW;
    }
    return bits;
}

__attribute__((format(printf, 3, 4)))
static unsigned noticef(struct nb_ui *ui, unsigned bits, const char *fmt, ...)
{
    va_list ap;

    va_start(ap, fmt);
    vsnprintf(ui->notice, sizeof(ui->notice), fmt, ap);
    va_end(ap);
    return bits | NB_UI_NOTICE;
}

static unsigned res_notice(struct nb_ui *ui, unsigned bits)
{
    char r[24];

    if (!(bits & NB_UI_RES)) {
        return bits;            /* nothing changed: nothing to announce */
    }
    if (ui->st->cur.res_w) {
        nb_view_res_str(&ui->st->cur, r, sizeof(r));
    } else {
        snprintf(r, sizeof(r), "Native");
    }
    return noticef(ui, bits, "Guest resolution: %s", r);
}

static unsigned act(struct nb_ui *ui, const struct nb_ui_env *env,
                    const struct nb_ui_item *it)
{
    struct nb_vstore *st = ui->st;
    struct nb_view old = st->cur, *v = &st->cur;
    int code = (it->id << 8) | (it->arg & 0xff);
    unsigned bits = 0;
    char s[48];

    (void)env;
    /* any click elsewhere cancels a pending confirmation */
    if (ui->confirm && ui->confirm != code) {
        ui->confirm = 0;
        bits |= NB_UI_REDRAW;
    }
    if (ui->focus && ui->focus != it->id) {
        ui->focus = 0;
        ui->text[0] = '\0';
        bits |= NB_UI_REDRAW;
    }
    switch (it->id) {
    case NB_UI_ID_CLOSE:
        return nb_ui_set_open(ui, env, false);
    case NB_UI_ID_RES_NATIVE:
        v->res_w = v->res_h = 0;
        return (bits | res_notice(ui, diff(&old, v))) | NB_UI_REDRAW;
    case NB_UI_ID_RES_PRESET:
        v->res_w = nb_res_preset[it->arg][0];
        v->res_h = nb_res_preset[it->arg][1];
        return (bits | res_notice(ui, diff(&old, v))) | NB_UI_REDRAW;
    case NB_UI_ID_RES_RECENT:
        if (it->arg < st->nrecent) {
            v->res_w = st->recent[it->arg][0];
            v->res_h = st->recent[it->arg][1];
        }
        return (bits | res_notice(ui, diff(&old, v))) | NB_UI_REDRAW;
    case NB_UI_ID_RES_FIELD:
    case NB_UI_ID_PROF_FIELD:
        if (ui->focus != it->id) {
            ui->focus = it->id;
            ui->text[0] = '\0';
            if (it->id == NB_UI_ID_RES_FIELD && v->res_w && v->res_h) {
                nb_view_res_str(v, ui->text, sizeof(ui->text));
            }
        }
        return bits | NB_UI_REDRAW;
    case NB_UI_ID_AREA:
        v->area = it->arg;
        if (it->arg == NB_AREA_CUSTOM) {
            nb_view_area_str(v, s, sizeof(s));
        } else {
            snprintf(s, sizeof(s), "%s", nb_view_area_label(it->arg));
        }
        return bits | noticef(ui, diff(&old, v), "Area: %s", s) | NB_UI_REDRAW;
    case NB_UI_ID_AREA_RESET:
        v->area = NB_AREA_FULL;
        return bits | noticef(ui, diff(&old, v), "Area: %s", "Full window") |
               NB_UI_REDRAW;
    case NB_UI_ID_SCALE:
        v->scale = it->arg;
        return bits | noticef(ui, diff(&old, v), "Scale: %s",
                              nb_view_scale_label(it->arg)) | NB_UI_REDRAW;
    case NB_UI_ID_FILTER:
        v->filter = it->arg;
        return bits | noticef(ui, diff(&old, v), "Filter: %s",
                              it->arg == NB_FILTER_NEAREST ? "Sharp"
                                                           : "Smooth") |
               NB_UI_REDRAW;
    case NB_UI_ID_PROF_LOAD:
        if (nb_vstore_profile_load(st, it->arg) == 0) {
            return bits | noticef(ui, diff(&old, v) | NB_UI_REDRAW,
                                  "Profile: %s", st->prof[it->arg].name);
        }
        return bits | NB_UI_REDRAW;
    case NB_UI_ID_PROF_DEL:
        if (ui->confirm != code) {
            ui->confirm = code;
            return bits | NB_UI_REDRAW;
        }
        ui->confirm = 0;
        snprintf(s, sizeof(s), "%s", st->prof[it->arg].name);
        nb_vstore_profile_delete(st, it->arg);
        nb_ui_layout(ui, env, NULL, NULL);
        return noticef(ui, bits | NB_UI_SAVE | NB_UI_REDRAW,
                       "Deleted profile: %.40s", s);
    case NB_UI_ID_STATS:
        return bits | NB_UI_STATS | NB_UI_REDRAW;
    case NB_UI_ID_FULLSCREEN:
        return bits | NB_UI_FULLSCREEN | NB_UI_REDRAW;
    case NB_UI_ID_VM_REBOOT:
    case NB_UI_ID_VM_SHUTDOWN:
        if (ui->confirm != code) {
            ui->confirm = code;
            return bits | NB_UI_REDRAW;
        }
        ui->confirm = 0;
        return bits | NB_UI_REDRAW |
               (it->id == NB_UI_ID_VM_REBOOT ? NB_UI_VM_REBOOT
                                              : NB_UI_VM_SHUTDOWN);
    default:
        return bits;
    }
}

/* ── panel input ────────────────────────────────────────────────────────── */

static int panel_hit(const struct nb_ui *ui, double x, double y)
{
    double cy = y + ui->scroll;
    int i;

    if (x < 0 || y < 0 || x >= ui->lw || y >= ui->lh) {
        return -1;
    }
    for (i = 0; i < ui->nitem; i++) {
        const struct nb_rect *r = &ui->item[i].r;

        if (x >= r->x && x < r->x + r->w && cy >= r->y && cy < r->y + r->h) {
            return i;
        }
    }
    return -1;
}

unsigned nb_ui_panel_motion(struct nb_ui *ui, const struct nb_ui_env *env,
                            double x, double y)
{
    int h;

    (void)env;
    ui->px = x;
    ui->py = y;
    h = panel_hit(ui, x, y);
    if (h == ui->hover) {
        return 0;
    }
    ui->hover = h;
    return NB_UI_REDRAW;
}

unsigned nb_ui_panel_button(struct nb_ui *ui, const struct nb_ui_env *env,
                            bool down)
{
    int h = panel_hit(ui, ui->px, ui->py);
    unsigned bits;

    if (down) {
        ui->press = h;
        if (h < 0 && (ui->confirm || ui->focus)) {
            ui->confirm = 0;
            ui->focus = 0;
            ui->text[0] = '\0';
        }
        return NB_UI_REDRAW;
    }
    if (h < 0 || h != ui->press || ui->item[h].disabled) {
        ui->press = -1;
        return NB_UI_REDRAW;
    }
    ui->press = -1;
    {
        struct nb_ui_item it = ui->item[h];

        bits = act(ui, env, &it);
    }
    if (ui->open) {
        nb_ui_layout(ui, env, NULL, NULL);
        ui->hover = panel_hit(ui, ui->px, ui->py);
    }
    return bits;
}

unsigned nb_ui_panel_wheel(struct nb_ui *ui, const struct nb_ui_env *env,
                           int steps)
{
    int old = ui->scroll, max = ui->content_h - ui->lh;

    (void)env;
    ui->scroll += steps * 40;
    if (ui->scroll > max) {
        ui->scroll = max;
    }
    if (ui->scroll < 0) {
        ui->scroll = 0;
    }
    if (ui->scroll == old) {
        return 0;
    }
    ui->hover = panel_hit(ui, ui->px, ui->py);
    return NB_UI_REDRAW;
}

unsigned nb_ui_panel_leave(struct nb_ui *ui)
{
    if (ui->hover < 0 && ui->press < 0) {
        return 0;
    }
    ui->hover = ui->press = -1;
    return NB_UI_REDRAW;
}

/* ── area editor ────────────────────────────────────────────────────────── */

#define GRAB 8.0

int nb_ui_area_hit(const struct nb_rect *a, double wx, double wy)
{
    double x0 = a->x, y0 = a->y, x1 = a->x + a->w, y1 = a->y + a->h;
    bool l, r, t, b;

    if (wx < x0 - GRAB || wx >= x1 + GRAB || wy < y0 - GRAB ||
        wy >= y1 + GRAB) {
        return NB_UI_CUR_ARROW;
    }
    l = wx < x0 + GRAB;
    r = wx >= x1 - GRAB;
    t = wy < y0 + GRAB;
    b = wy >= y1 - GRAB;
    if (l && r) {               /* tiny: the nearer edge */
        if (wx - x0 < x1 - wx) { r = false; } else { l = false; }
    }
    if (t && b) {
        if (wy - y0 < y1 - wy) { b = false; } else { t = false; }
    }
    if (t && l) { return NB_UI_CUR_NW; }
    if (t && r) { return NB_UI_CUR_NE; }
    if (b && l) { return NB_UI_CUR_SW; }
    if (b && r) { return NB_UI_CUR_SE; }
    if (t) { return NB_UI_CUR_N; }
    if (b) { return NB_UI_CUR_S; }
    if (l) { return NB_UI_CUR_W; }
    if (r) { return NB_UI_CUR_E; }
    return NB_UI_CUR_MOVE;
}

static int px_of(int l, unsigned s120)
{
    return (int)(((long)l * (s120 ? s120 : 120) + 60) / 120);
}

unsigned nb_ui_area_motion(struct nb_ui *ui, const struct nb_ui_env *env,
                           double wx, double wy)
{
    struct nb_view old = ui->st->cur, *v = &ui->st->cur;
    double k = (env->s120 ? env->s120 : 120) / 120.0;
    int W = px_of(env->win_w, env->s120), H = px_of(env->win_h, env->s120);
    struct nb_rect r;
    int dx, dy, d = ui->drag;
    bool snx = false, sny = false;
    unsigned bits;

    ui->px = wx;
    ui->py = wy;
    if (d < 0) {
        struct nb_rect a;
        int h;

        nb_view_area(v, env->win_w, env->win_h, env->s120, &a);
        h = nb_ui_area_hit(&a, wx, wy);
        if (h == ui->hot) {
            return 0;
        }
        ui->hot = h;
        return NB_UI_CURSOR;
    }
    dx = (int)lround((wx - ui->drag_x0) * k);
    dy = (int)lround((wy - ui->drag_y0) * k);
    r = ui->drag_r0;
    if (d == NB_UI_CUR_MOVE) {
        r.x += dx;
        r.y += dy;
        if (r.x < 0) { r.x = 0; }
        if (r.y < 0) { r.y = 0; }
        if (r.x + r.w > W) { r.x = W - r.w; }
        if (r.y + r.h > H) { r.y = H - r.h; }
        if (abs(r.x + r.w / 2 - W / 2) <= 12) {
            r.x = (W - r.w) / 2;
            snx = true;
        }
        if (abs(r.y + r.h / 2 - H / 2) <= 12) {
            r.y = (H - r.h) / 2;
            sny = true;
        }
    } else {
        int x0 = r.x, y0 = r.y, x1 = r.x + r.w, y1 = r.y + r.h;

        if (d == NB_UI_CUR_W || d == NB_UI_CUR_NW || d == NB_UI_CUR_SW) {
            x0 += dx;
            if (x0 < 0) { x0 = 0; }
            if (x0 > x1 - 64) { x0 = x1 - 64; }
        }
        if (d == NB_UI_CUR_E || d == NB_UI_CUR_NE || d == NB_UI_CUR_SE) {
            x1 += dx;
            if (x1 > W) { x1 = W; }
            if (x1 < x0 + 64) { x1 = x0 + 64; }
        }
        if (d == NB_UI_CUR_N || d == NB_UI_CUR_NW || d == NB_UI_CUR_NE) {
            y0 += dy;
            if (y0 < 0) { y0 = 0; }
            if (y0 > y1 - 36) { y0 = y1 - 36; }
        }
        if (d == NB_UI_CUR_S || d == NB_UI_CUR_SW || d == NB_UI_CUR_SE) {
            y1 += dy;
            if (y1 > H) { y1 = H; }
            if (y1 < y0 + 36) { y1 = y0 + 36; }
        }
        r.x = x0;
        r.y = y0;
        r.w = x1 - x0;
        r.h = y1 - y0;
    }
    v->area = NB_AREA_CUSTOM;
    v->cw = r.w;
    v->ch = r.h;
    v->cx = r.x;
    v->cy = r.y;
    v->ccenter = snx && sny;
    bits = diff(&old, v) & (NB_UI_VIEW | NB_UI_RES);
    return bits;
}

unsigned nb_ui_area_button(struct nb_ui *ui, const struct nb_ui_env *env,
                           bool down)
{
    unsigned bits = 0;

    if (down) {
        if (ui->confirm || ui->focus) {
            ui->confirm = 0;
            ui->focus = 0;
            ui->text[0] = '\0';
            bits |= NB_UI_REDRAW;
        }
        if (ui->hot == NB_UI_CUR_ARROW) {
            return bits;
        }
        ui->drag = ui->hot;
        ui->drag_x0 = ui->px;
        ui->drag_y0 = ui->py;
        nb_view_area_px(&ui->st->cur, env->win_w, env->win_h, env->s120,
                        &ui->drag_r0);
        return bits;
    }
    if (ui->drag < 0) {
        return 0;
    }
    ui->drag = -1;
    {
        char s[40];
        struct nb_rect a;

        nb_view_area_str(&ui->st->cur, s, sizeof(s));
        snprintf(ui->notice, sizeof(ui->notice), "Area: %s", s);
        nb_view_area(&ui->st->cur, env->win_w, env->win_h, env->s120, &a);
        ui->hot = nb_ui_area_hit(&a, ui->px, ui->py);
    }
    return NB_UI_SAVE | NB_UI_REDRAW | NB_UI_NOTICE | NB_UI_CURSOR;
}

int nb_ui_area_cursor(const struct nb_ui *ui)
{
    return ui->drag >= 0 ? ui->drag : ui->hot;
}

/* ── keys ───────────────────────────────────────────────────────────────── */

static char key_char(unsigned code, bool shift, bool res_field)
{
    static const char row1[] = "qwertyuiop", row2[] = "asdfghjkl",
                      row3[] = "zxcvbnm";
    char c = 0;

    if (code >= KEY_1 && code <= KEY_9) {
        c = (char)('1' + (code - KEY_1));
    } else if (code == KEY_0) {
        c = '0';
    } else if (code == KEY_KP0) {
        c = '0';
    } else if (code == KEY_KP1 || code == KEY_KP2 || code == KEY_KP3) {
        c = (char)('1' + (code - KEY_KP1));
    } else if (code >= KEY_KP4 && code <= KEY_KP6) {
        c = (char)('4' + (code - KEY_KP4));
    } else if (code >= KEY_KP7 && code <= KEY_KP9) {
        c = (char)('7' + (code - KEY_KP7));
    } else if (code >= KEY_Q && code <= KEY_P) {
        c = row1[code - KEY_Q];
    } else if (code >= KEY_A && code <= KEY_L) {
        c = row2[code - KEY_A];
    } else if (code >= KEY_Z && code <= KEY_M) {
        c = row3[code - KEY_Z];
    } else if (code == KEY_SPACE) {
        c = ' ';
    } else if (code == KEY_MINUS) {
        c = shift ? '_' : '-';
    } else if (code == KEY_SEMICOLON && shift) {
        c = ':';
    } else if (code == KEY_DOT) {
        c = '.';
    }
    if (res_field) {
        if (c == 'X') {
            c = 'x';
        }
        return (c >= '0' && c <= '9') || c == 'x' ? c : 0;
    }
    if (shift && c >= 'a' && c <= 'z') {
        c = (char)(c - 'a' + 'A');
    }
    if (shift && c >= '0' && c <= '9' && code < KEY_KP7) {
        return 0;               /* !@#... are not name characters */
    }
    return c;
}

unsigned nb_ui_key(struct nb_ui *ui, const struct nb_ui_env *env,
                   unsigned code, bool shift, bool down)
{
    size_t len;

    if (!down || !ui->open) {
        return 0;
    }
    if (code == KEY_ESC) {
        if (ui->focus || ui->confirm) {
            ui->focus = 0;
            ui->confirm = 0;
            ui->text[0] = '\0';
            return NB_UI_REDRAW;
        }
        return nb_ui_set_open(ui, env, false);
    }
    if (!ui->focus) {
        return 0;
    }
    len = strlen(ui->text);
    if (code == KEY_BACKSPACE) {
        if (len) {
            ui->text[len - 1] = '\0';
        }
        return NB_UI_REDRAW;
    }
    if (code == KEY_ENTER || code == KEY_KPENTER) {
        struct nb_vstore *st = ui->st;
        struct nb_view old = st->cur;
        unsigned w, h, bits;

        if (ui->focus == NB_UI_ID_RES_FIELD) {
            if (nb_view_parse_res(ui->text, &w, &h) != 0 || !w || !h ||
                w > 8192 || h > 8192) {
                return noticef(ui, NB_UI_REDRAW, "Not a resolution: %s",
                               ui->text[0] ? ui->text : "(empty)");
            }
            st->cur.res_w = w;
            st->cur.res_h = h;
            nb_vstore_note_res(st, w, h);
            ui->focus = 0;
            ui->text[0] = '\0';
            bits = diff(&old, &st->cur) | NB_UI_SAVE | NB_UI_REDRAW;
            nb_ui_layout(ui, env, NULL, NULL);
            return res_notice(ui, bits);
        }
        if (ui->focus == NB_UI_ID_PROF_FIELD) {
            char name[NB_UI_TEXT_MAX];

            snprintf(name, sizeof(name), "%s", ui->text);
            if (nb_vstore_profile_save(st, name) < 0) {
                return noticef(ui, NB_UI_REDRAW, "%s",
                               name[0] ? "Too many profiles"
                                       : "Type a name first");
            }
            ui->focus = 0;
            ui->text[0] = '\0';
            nb_ui_layout(ui, env, NULL, NULL);
            return noticef(ui, NB_UI_SAVE | NB_UI_REDRAW, "Saved profile: %s",
                           name);
        }
        return 0;
    }
    {
        char c = key_char(code, shift, ui->focus == NB_UI_ID_RES_FIELD);

        if (!c || len + 1 >= sizeof(ui->text) ||
            (ui->focus == NB_UI_ID_PROF_FIELD &&
             len + 1 >= NB_PROFILE_NAME)) {
            return 0;
        }
        ui->text[len] = c;
        ui->text[len + 1] = '\0';
        return NB_UI_REDRAW;
    }
}

/* ── painting ───────────────────────────────────────────────────────────── */

void nb_ui_paint_panel(struct nb_ui *ui, const struct nb_ui_env *env,
                       uint32_t *px, int pw, int ph, int stride_px)
{
    struct pnt p = { .px = px, .w = pw, .h = ph, .stride = stride_px,
                     .k = (env->s120 ? env->s120 : 120) / 120.0,
                     .oy = 0, .alpha = 1.0 };
    int y;

    for (y = 0; y < ph; y++) {
        memset(px + (size_t)y * stride_px, 0, (size_t)pw * 4);
    }
    nb_ui_layout(ui, env, NULL, NULL);
    if (env->translucent) {
        p_rrect(&p, 0, 0, ui->lw, ui->lh, UI_RADIUS, C_BG, 0);
        p_rrect(&p, 0, 0, ui->lw, ui->lh, UI_RADIUS, C_EDGE, p.k);
    } else {
        int x;

        for (y = 0; y < ph; y++) {
            for (x = 0; x < pw; x++) {
                px[(size_t)y * stride_px + x] = C_BG_OPAQUE;
            }
        }
        p_rrect(&p, 0, 0, ui->lw, ui->lh, 0, C_EDGE, p.k);
    }
    p.oy = ui->scroll;
    build(ui, env, &p);
    p.oy = 0;
    /* scroll indicator */
    if (ui->content_h > ui->lh && ui->lh > 0) {
        double track = ui->lh - 2 * UI_RADIUS;
        double th = track * ui->lh / ui->content_h;
        double ty = UI_RADIUS + (track - th) * ui->scroll /
                                (ui->content_h - ui->lh);

        p_rrect(&p, ui->lw - 6, ty, 3, th, 1.5, 0x50ffffffu, 0);
    }
}

void nb_ui_notice_size(const char *txt, int *lw, int *lh)
{
    *lw = text_w(txt ? txt : "", F_NOTICE, true, 0) + 2 * 18 + 14;
    *lh = 36;
}

void nb_ui_paint_notice(const char *txt, unsigned s120, uint32_t *px, int pw,
                        int ph, int stride_px, bool opaque)
{
    struct pnt p = { .px = px, .w = pw, .h = ph, .stride = stride_px,
                     .k = (s120 ? s120 : 120) / 120.0, .alpha = 1.0 };
    int lw, lh, y;

    if (!txt) {
        txt = "";
    }
    nb_ui_notice_size(txt, &lw, &lh);
    for (y = 0; y < ph; y++) {
        int x;

        for (x = 0; x < pw; x++) {
            px[(size_t)y * stride_px + x] = opaque ? C_BG_OPAQUE : 0;
        }
    }
    if (!opaque) {
        p_rrect(&p, 0, 0, lw, lh, lh / 2.0, C_BG, 0);
    }
    p_rrect(&p, 0, 0, lw, lh, opaque ? 0 : lh / 2.0, 0x603fb950u, p.k);
    p_circle(&p, 18 + 3, lh / 2.0, 4, C_GREEN);
    p_text(&p, 18 + 14, lh / 2.0, txt, F_NOTICE, true, C_TEXT, 0, 0);
}

void nb_ui_button_size(int *lw, int *lh)
{
    *lw = 120;
    *lh = 28;
}

void nb_ui_paint_button(unsigned s120, uint32_t *px, int pw, int ph,
                        int stride_px, double alpha, bool hot, bool opaque)
{
    struct pnt p = { .px = px, .w = pw, .h = ph, .stride = stride_px,
                     .k = (s120 ? s120 : 120) / 120.0,
                     .alpha = alpha < 0 ? 0 : alpha > 1 ? 1 : alpha };
    int lw, lh, y;

    nb_ui_button_size(&lw, &lh);
    for (y = 0; y < ph; y++) {
        int x;

        for (x = 0; x < pw; x++) {
            px[(size_t)y * stride_px + x] = 0;
        }
    }
    if (opaque) {
        /* No blending layer underneath (X11 without a compositor): a solid
         * rectangle; the fade then only dims what is drawn on it. */
        for (y = 0; y < ph; y++) {
            int x;

            for (x = 0; x < pw; x++) {
                px[(size_t)y * stride_px + x] = C_BG_OPAQUE;
            }
        }
    }
    p_rrect(&p, 0, 0, lw, lh, opaque ? 0 : lh / 2.0,
            hot ? 0xf0161b22u : C_BG, 0);
    p_rrect(&p, 0, 0, lw, lh, opaque ? 0 : lh / 2.0,
            hot ? C_GREEN : 0x603fb950u, p.k);
    p_circle(&p, 16, lh / 2.0, 4, hot ? C_GREEN_HI : C_GREEN);
    p_text(&p, 28, lh / 2.0, "CONDUIT", F_BUTTON, true,
           hot ? C_GREEN_HI : C_TEXT, 0, 2.0);
}
