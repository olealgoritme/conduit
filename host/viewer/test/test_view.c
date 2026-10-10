/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * test_view.c -- picture placement and pointer mapping (nb_view.c).
 *
 * The rule under test: a pointer position maps to the guest pixel that is
 * DRAWN under it, for every scale mode, area, offset and output scale, and
 * over the bars it rests on the nearest edge of the picture.
 */
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include "nb_view.h"

static int fails, checks;

#define CHECK(c, ...) do { \
        checks++; \
        if (!(c)) { \
            fails++; \
            fprintf(stderr, "FAIL %s:%d: ", __FILE__, __LINE__); \
            fprintf(stderr, __VA_ARGS__); \
            fputc('\n', stderr); \
        } \
    } while (0)

static struct nb_view view(int scale, int area)
{
    struct nb_view v;

    nb_view_defaults(&v);
    v.scale = scale;
    v.area = area;
    return v;
}

static void rect_eq(const struct nb_rect *r, int x, int y, int w, int h,
                    const char *what)
{
    CHECK(r->x == x && r->y == y && r->w == w && r->h == h,
          "%s: got %d,%d %dx%d want %d,%d %dx%d", what, r->x, r->y, r->w,
          r->h, x, y, w, h);
}

/*
 * The forward transform: the window position at which guest pixel (gx, gy)'s
 * CENTRE is drawn, derived the way a compositor draws vis from the source
 * rect.  Mapping that position back must give (gx, gy) -- the inverse of the
 * draw, not of some other formula.
 */
static void draw_pos(const struct nb_place *p, double gx, double gy,
                     double *wx, double *wy)
{
    *wx = p->vis.x + (gx + 0.5 - p->sx) * p->vis.w / p->sw;
    *wy = p->vis.y + (gy + 0.5 - p->sy) * p->vis.h / p->sh;
}

static void roundtrip(const struct nb_view *v, int bw, int bh, int ww, int wh,
                      unsigned s120, const char *what)
{
    struct nb_place p;
    int i, bad = 0;

    nb_view_place(v, bw, bh, ww, wh, s120, &p);
    for (i = 0; i < 400; i++) {
        int gx = (int)((long)i * 7919 % bw), gy = (int)((long)i * 104729 % bh);
        double wx, wy;
        int mx, my;

        draw_pos(&p, gx, gy, &wx, &wy);
        if (wx < p.vis.x || wy < p.vis.y || wx >= p.vis.x + p.vis.w ||
            wy >= p.vis.y + p.vis.h) {
            continue;           /* cropped away: not on screen */
        }
        nb_view_map(&p, bw, bh, wx, wy, &mx, &my);
        /* A pixel drawn smaller than a window unit can only be resolved to
         * within the window unit; allow that and nothing more. */
        {
            double ux = (double)bw / p.pic.w, uy = (double)bh / p.pic.h;
            double tx = ux > 1 ? ux : 0, ty = uy > 1 ? uy : 0;

            if (abs(mx - gx) > tx || abs(my - gy) > ty) {
                if (bad++ < 3) {
                    fprintf(stderr, "  %s: guest %d,%d drawn at %.2f,%.2f "
                            "maps to %d,%d\n", what, gx, gy, wx, wy, mx, my);
                }
            }
        }
    }
    CHECK(bad == 0, "%s: %d round-trip mismatches", what, bad);
}

static void test_fit(void)
{
    struct nb_view v = view(NB_SCALE_ASPECT, NB_AREA_FULL);
    struct nb_place p;

    /* 16:9 in 16:9: exactly the window, no bars. */
    nb_view_place(&v, 1920, 1080, 2560, 1440, 120, &p);
    rect_eq(&p.pic, 0, 0, 2560, 1440, "fit exact");
    CHECK(!p.cropped, "fit exact cropped");

    /* 16:9 in 32:9: pillarboxed, centred. */
    nb_view_place(&v, 1920, 1080, 5120, 1440, 120, &p);
    rect_eq(&p.pic, 1280, 0, 2560, 1440, "fit pillarbox");

    /* 4:3 in 16:9 */
    nb_view_place(&v, 1280, 960, 1920, 1080, 120, &p);
    rect_eq(&p.pic, 240, 0, 1440, 1080, "fit 4:3");
    rect_eq(&p.vis, 240, 0, 1440, 1080, "fit 4:3 vis");

    roundtrip(&v, 1920, 1080, 5120, 1440, 120, "fit 32:9");
    roundtrip(&v, 1280, 960, 1920, 1080, 120, "fit 4:3");
    roundtrip(&v, 3840, 2160, 1280, 720, 120, "fit downscale");
}

static void test_stretch(void)
{
    struct nb_view v = view(NB_SCALE_STRETCH, NB_AREA_FULL);
    struct nb_place p;
    int gx, gy;

    nb_view_place(&v, 1280, 960, 1920, 1080, 120, &p);
    rect_eq(&p.pic, 0, 0, 1920, 1080, "stretch");
    nb_view_map(&p, 1280, 960, 960, 540, &gx, &gy);
    CHECK(gx == 640 && gy == 480, "stretch centre -> %d,%d", gx, gy);
    nb_view_map(&p, 1280, 960, 1919.9, 1079.9, &gx, &gy);
    CHECK(gx == 1279 && gy == 959, "stretch corner -> %d,%d", gx, gy);
    roundtrip(&v, 1280, 960, 1920, 1080, 120, "stretch 4:3 -> 16:9");
}

static void test_integer(void)
{
    struct nb_view v = view(NB_SCALE_INTEGER, NB_AREA_FULL);
    struct nb_place p;

    /* 640x480 in 1920x1080: x2 (x3 would be 1440 tall). */
    nb_view_place(&v, 640, 480, 1920, 1080, 120, &p);
    rect_eq(&p.pic, 320, 60, 1280, 960, "integer x2");
    roundtrip(&v, 640, 480, 1920, 1080, 120, "integer x2");

    /* 1920x1080 in 3840x2160 at output scale 2: window is 1920x1080
     * logical, 3840x2160 output -> x2 in output pixels = the window. */
    nb_view_place(&v, 1920, 1080, 1920, 1080, 240, &p);
    rect_eq(&p.pic, 0, 0, 1920, 1080, "integer hidpi x2");

    /* 1280x720 at 1.5: window 2560x1440 logical = 3840x2160 output -> x3
     * output = 3840x2160 = the whole window. */
    nb_view_place(&v, 1280, 720, 2560, 1440, 180, &p);
    rect_eq(&p.pic, 0, 0, 2560, 1440, "integer 1.5 x3");

    /* Larger than the window: falls back to fit. */
    nb_view_place(&v, 3840, 2160, 1920, 1080, 120, &p);
    rect_eq(&p.pic, 0, 0, 1920, 1080, "integer falls back to fit");
    roundtrip(&v, 800, 600, 1707, 960, 180, "integer fractional");
}

static void test_none(void)
{
    struct nb_view v = view(NB_SCALE_NONE, NB_AREA_FULL);
    struct nb_place p;
    int gx, gy;
    bool in;

    nb_view_place(&v, 1280, 720, 1920, 1080, 120, &p);
    rect_eq(&p.pic, 320, 180, 1280, 720, "none centred");
    CHECK(!p.cropped, "none small not cropped");
    roundtrip(&v, 1280, 720, 1920, 1080, 120, "none");

    /* At output scale 2, 1:1 OUTPUT pixels: 1280x720 guest px = 640x360
     * logical. */
    nb_view_place(&v, 1280, 720, 1920, 1080, 240, &p);
    rect_eq(&p.pic, 640, 360, 640, 360, "none hidpi");
    roundtrip(&v, 1280, 720, 1920, 1080, 240, "none hidpi");

    /* Larger than the window: centred and cropped. */
    nb_view_place(&v, 2560, 1440, 1920, 1080, 120, &p);
    rect_eq(&p.pic, -320, -180, 2560, 1440, "none crop pic");
    rect_eq(&p.vis, 0, 0, 1920, 1080, "none crop vis");
    CHECK(p.cropped, "none crop flag");
    CHECK(p.sx == 320 && p.sy == 180 && p.sw == 1920 && p.sh == 1080,
          "none crop src %.1f,%.1f %.1fx%.1f", p.sx, p.sy, p.sw, p.sh);
    in = nb_view_map(&p, 2560, 1440, 0, 0, &gx, &gy);
    CHECK(in && gx == 320 && gy == 180, "none crop origin -> %d,%d", gx, gy);
    roundtrip(&v, 2560, 1440, 1920, 1080, 120, "none crop");
}

static void test_areas(void)
{
    struct nb_view v = view(NB_SCALE_STRETCH, NB_AREA_16_9);
    struct nb_place p;
    struct nb_rect a;

    /* 16:9 area on a 32:9 window. */
    nb_view_area(&v, 5120, 1440, 120, &a);
    rect_eq(&a, 1280, 0, 2560, 1440, "area 16:9 on 32:9");
    v.area = NB_AREA_4_3;
    nb_view_area(&v, 1920, 1080, 120, &a);
    rect_eq(&a, 240, 0, 1440, 1080, "area 4:3");
    v.area = NB_AREA_21_9;
    nb_view_area(&v, 1920, 1080, 120, &a);
    CHECK(a.w == 1920 && a.h == 822 && a.x == 0 && a.y == 129,
          "area 21:9 %d,%d %dx%d", a.x, a.y, a.w, a.h);
    v.area = NB_AREA_16_10;
    nb_view_area(&v, 1920, 1080, 120, &a);
    rect_eq(&a, 96, 0, 1728, 1080, "area 16:10");

    /* Stretched 4:3 guest into a 4:3 area of a 16:9 window (the CS2 setup). */
    v.area = NB_AREA_4_3;
    nb_view_place(&v, 1280, 960, 1920, 1080, 120, &p);
    rect_eq(&p.pic, 240, 0, 1440, 1080, "stretch in 4:3 area");
    roundtrip(&v, 1280, 960, 1920, 1080, 120, "stretch in 4:3 area");

    /* Fit a 16:9 guest into a 4:3 area: letterboxed inside the area. */
    v.scale = NB_SCALE_ASPECT;
    nb_view_place(&v, 1920, 1080, 1920, 1080, 120, &p);
    rect_eq(&p.pic, 240, 135, 1440, 810, "fit inside 4:3 area");
    roundtrip(&v, 1920, 1080, 1920, 1080, 120, "fit inside 4:3 area");
}

static void test_custom(void)
{
    struct nb_view v;
    struct nb_place p;
    struct nb_rect a;
    int gx, gy;

    nb_view_defaults(&v);
    CHECK(nb_view_parse_area("1600x900", &v) == 0, "parse custom");
    CHECK(v.area == NB_AREA_CUSTOM && v.cw == 1600 && v.ch == 900 &&
          v.ccenter, "custom centred");
    nb_view_area(&v, 1920, 1080, 120, &a);
    rect_eq(&a, 160, 90, 1600, 900, "custom centred rect");

    CHECK(nb_view_parse_area("1600x900+10+20", &v) == 0, "parse offset");
    CHECK(!v.ccenter && v.cx == 10 && v.cy == 20, "offset parsed");
    nb_view_area(&v, 1920, 1080, 120, &a);
    rect_eq(&a, 10, 20, 1600, 900, "custom offset rect");

    /* Offset past the edge is pulled back inside. */
    nb_view_parse_area("1600x900+1000+1000", &v);
    nb_view_area(&v, 1920, 1080, 120, &a);
    rect_eq(&a, 320, 180, 1600, 900, "custom clamped");

    /* Larger than the window: clipped to it. */
    nb_view_parse_area("4000x3000", &v);
    nb_view_area(&v, 1920, 1080, 120, &a);
    rect_eq(&a, 0, 0, 1920, 1080, "custom clipped");

    /* Output pixels at scale 1.5: 1200x600 px = 800x400 logical. */
    nb_view_parse_area("1200x600+300+150", &v);
    nb_view_area(&v, 1280, 720, 180, &a);
    rect_eq(&a, 200, 100, 800, 400, "custom at 1.5");
    v.scale = NB_SCALE_STRETCH;
    nb_view_place(&v, 1920, 1080, 1280, 720, 180, &p);
    nb_view_map(&p, 1920, 1080, 200, 100, &gx, &gy);
    CHECK(gx == 0 && gy == 0, "custom 1.5 origin -> %d,%d", gx, gy);
    nb_view_map(&p, 1920, 1080, 600, 300, &gx, &gy);
    CHECK(gx == 960 && gy == 540, "custom 1.5 centre -> %d,%d", gx, gy);
    roundtrip(&v, 1920, 1080, 1280, 720, 180, "custom 1.5 stretch");

    CHECK(nb_view_parse_area("0x900", &v) != 0, "reject zero");
    CHECK(nb_view_parse_area("16:8", &v) != 0, "reject unknown preset");
    CHECK(nb_view_parse_area("100x100+5", &v) != 0, "reject half offset");
    CHECK(nb_view_parse_area("100x100-5-5", &v) != 0, "reject negative");
}

static void test_clamp(void)
{
    struct nb_view v = view(NB_SCALE_ASPECT, NB_AREA_FULL);
    struct nb_place p;
    int gx, gy;
    bool in;

    /* 4:3 in 16:9: bars of 240 on each side. */
    nb_view_place(&v, 1280, 960, 1920, 1080, 120, &p);
    in = nb_view_map(&p, 1280, 960, 10, 500, &gx, &gy);
    CHECK(!in && gx == 0, "left bar clamps to x=0 (got %d, in %d)", gx, in);
    CHECK(gy == (int)(500.0 * 960 / 1080), "left bar keeps y (%d)", gy);
    in = nb_view_map(&p, 1280, 960, 1900, 1070, &gx, &gy);
    CHECK(!in && gx == 1279 && gy == (int)(1070.0 * 960 / 1080),
          "right bar clamps (%d,%d)", gx, gy);
    in = nb_view_map(&p, 1280, 960, -50, -50, &gx, &gy);
    CHECK(!in && gx == 0 && gy == 0, "outside top-left (%d,%d)", gx, gy);
    in = nb_view_map(&p, 1280, 960, 5000, 5000, &gx, &gy);
    CHECK(!in && gx == 1279 && gy == 959, "outside bottom-right (%d,%d)",
          gx, gy);
    in = nb_view_map(&p, 1280, 960, 240, 0, &gx, &gy);
    CHECK(in && gx == 0 && gy == 0, "first picture pixel");
    in = nb_view_map(&p, 1280, 960, 1679.99, 1079.99, &gx, &gy);
    CHECK(in && gx == 1279 && gy == 959, "last picture pixel (%d,%d)", gx,
          gy);
    in = nb_view_map(&p, 1280, 960, 1680, 500, &gx, &gy);
    CHECK(!in && gx == 1279, "first bar pixel is a bar");
}

static void test_fractional(void)
{
    static const unsigned scales[] = { 120, 150, 180, 210, 240 };
    static const int modes[] = { NB_SCALE_ASPECT, NB_SCALE_STRETCH,
                                 NB_SCALE_INTEGER, NB_SCALE_NONE };
    static const int areas[] = { NB_AREA_FULL, NB_AREA_21_9, NB_AREA_16_9,
                                 NB_AREA_4_3 };
    static const int guests[][2] = { { 1920, 1080 }, { 1280, 960 },
                                     { 3840, 2160 }, { 800, 600 } };
    unsigned si;
    int mi, ai, gi;

    for (si = 0; si < sizeof(scales) / sizeof(scales[0]); si++) {
        for (mi = 0; mi < 4; mi++) {
            for (ai = 0; ai < 4; ai++) {
                for (gi = 0; gi < 4; gi++) {
                    struct nb_view v = view(modes[mi], areas[ai]);
                    struct nb_place p;
                    char what[96];
                    int ww = 1707, wh = 961;

                    snprintf(what, sizeof(what), "s%u %s area%d %dx%d",
                             scales[si], nb_view_scale_name(modes[mi]),
                             areas[ai], guests[gi][0], guests[gi][1]);
                    nb_view_place(&v, guests[gi][0], guests[gi][1], ww, wh,
                                  scales[si], &p);
                    /* Visible part always inside the area and the window. */
                    CHECK(p.vis.x >= p.area.x && p.vis.y >= p.area.y &&
                          p.vis.x + p.vis.w <= p.area.x + p.area.w &&
                          p.vis.y + p.vis.h <= p.area.y + p.area.h,
                          "%s: vis outside area", what);
                    CHECK(p.area.x >= 0 && p.area.y >= 0 &&
                          p.area.x + p.area.w <= ww &&
                          p.area.y + p.area.h <= wh,
                          "%s: area outside window", what);
                    CHECK(p.sx >= 0 && p.sy >= 0 &&
                          p.sx + p.sw <= guests[gi][0] + 1e-6 &&
                          p.sy + p.sh <= guests[gi][1] + 1e-6,
                          "%s: source outside buffer", what);
                    roundtrip(&v, guests[gi][0], guests[gi][1], ww, wh,
                              scales[si], what);
                }
            }
        }
    }
}

static void test_guest_mode(void)
{
    struct nb_view v = view(NB_SCALE_ASPECT, NB_AREA_FULL);
    unsigned w, h;

    CHECK(!nb_view_guest_mode(&v, 1920, 1080, 120, &w, &h) && w == 1920 &&
          h == 1080, "native = window");
    CHECK(!nb_view_guest_mode(&v, 1707, 960, 180, &w, &h) && w == 2561 &&
          h == 1440, "native = output pixels (%ux%u)", w, h);
    v.area = NB_AREA_16_9;
    CHECK(!nb_view_guest_mode(&v, 5120, 1440, 120, &w, &h) && w == 2560 &&
          h == 1440, "native = area (%ux%u)", w, h);
    v.res_w = 1280;
    v.res_h = 960;
    CHECK(nb_view_guest_mode(&v, 5120, 1440, 120, &w, &h) && w == 1280 &&
          h == 960, "fixed");
}

static void test_names(void)
{
    int s, f;
    unsigned w, h;

    CHECK(nb_view_parse_scale("fit", &s) == 0 && s == NB_SCALE_ASPECT, "fit");
    CHECK(nb_view_parse_scale("aspect", &s) == 0 && s == NB_SCALE_ASPECT,
          "aspect");
    CHECK(nb_view_parse_scale("integer", &s) == 0 && s == NB_SCALE_INTEGER,
          "integer");
    CHECK(nb_view_parse_scale("centered", &s) == 0 && s == NB_SCALE_NONE,
          "centered");
    CHECK(nb_view_parse_scale("zoom", &s) != 0, "bad scale");
    CHECK(nb_view_parse_filter("nearest", &f) == 0 && f == NB_FILTER_NEAREST,
          "nearest");
    CHECK(nb_view_parse_filter("blur", &f) != 0, "bad filter");
    CHECK(nb_view_parse_res("native", &w, &h) == 0 && !w && !h, "native");
    CHECK(nb_view_parse_res("2560x1440", &w, &h) == 0 && w == 2560 &&
          h == 1440, "res");
    CHECK(nb_view_parse_res("2560x", &w, &h) != 0, "bad res");
    CHECK(nb_view_parse_res("2560x1440x2", &w, &h) != 0, "trailing res");
}

static void test_store(void)
{
    struct nb_vstore st, st2;
    char buf[4096], label[64];
    unsigned ch;

    nb_vstore_init(&st);
    st.cur.scale = NB_SCALE_STRETCH;
    nb_view_parse_area("1440x1080+240+0", &st.cur);
    st.cur.res_w = 1280;
    st.cur.res_h = 960;
    st.cur.filter = NB_FILTER_NEAREST;
    CHECK(nb_vstore_profile_save(&st, "CS2 4:3 stretched") == 0, "save prof");
    st.cur.scale = NB_SCALE_ASPECT;
    st.cur.area = NB_AREA_FULL;
    CHECK(nb_vstore_profile_save(&st, "  Desktop  ") == 1, "save prof 2");
    CHECK(!strcmp(st.prof[1].name, "Desktop"), "name trimmed");
    CHECK(nb_vstore_profile_save(&st, "bad|name") == 2 &&
          !strcmp(st.prof[2].name, "badname"), "bar stripped");
    nb_vstore_profile_delete(&st, 2);
    nb_vstore_note_res(&st, 1720, 1080);
    nb_vstore_note_res(&st, 1920, 1080);   /* preset: not recent */
    nb_vstore_note_res(&st, 1024, 768);
    nb_vstore_note_res(&st, 1720, 1080);   /* moves to the front */
    CHECK(st.nrecent == 2 && st.recent[0][0] == 1720 &&
          st.recent[1][0] == 1024, "recent order");

    nb_vstore_format(&st, buf, sizeof(buf));
    nb_vstore_init(&st2);
    nb_vstore_parse(&st2, buf);
    CHECK(!memcmp(&st.cur, &st2.cur, sizeof(st.cur)), "cur round trip:\n%s",
          buf);
    CHECK(st2.nprof == 2 && !strcmp(st2.prof[0].name, "CS2 4:3 stretched"),
          "profiles round trip");
    CHECK(!memcmp(&st.prof[0].v, &st2.prof[0].v, sizeof(st.prof[0].v)),
          "profile view round trip");
    CHECK(st2.nrecent == 2 && st2.recent[0][0] == 1720, "recent round trip");

    /* Garbage is skipped, not fatal. */
    nb_vstore_init(&st2);
    nb_vstore_parse(&st2, "scale=wobble\narea=99x\nres=1x\nnonsense\n"
                          "profile=|scale=fit\nfilter=nearest\n");
    CHECK(st2.cur.scale == NB_SCALE_ASPECT && st2.cur.area == NB_AREA_FULL &&
          st2.cur.res_w == 0 && st2.nprof == 0 &&
          st2.cur.filter == NB_FILTER_NEAREST, "garbage skipped");

    /* Save and load through a real file. */
    {
        char path[] = "/tmp/nb-view-test-XXXXXX";
        int fd = mkstemp(path);

        CHECK(fd >= 0, "mkstemp");
        if (fd >= 0) {
            close(fd);
            CHECK(nb_vstore_save(&st, path) == 0, "save");
            nb_vstore_init(&st2);
            CHECK(nb_vstore_load(&st2, path) == 0, "load");
            CHECK(!memcmp(&st.cur, &st2.cur, sizeof(st.cur)), "file trip");
            unlink(path);
            nb_vstore_init(&st2);
            CHECK(nb_vstore_load(&st2, path) == 0 &&
                  st2.cur.scale == NB_SCALE_ASPECT, "missing file = defaults");
        }
    }

    /* Profiles cycle and restore. */
    ch = nb_view_cycle_profile(&st, label, sizeof(label));
    CHECK(ch & NB_VIEW_CH_LAYOUT, "profile cycle relayouts");
    CHECK(!strcmp(label, "Profile: CS2 4:3 stretched") ||
          !strcmp(label, "Profile: Desktop"), "profile label %s", label);
}

static void test_cycles(void)
{
    struct nb_vstore st;
    char label[64];
    unsigned ch;
    int i;

    nb_vstore_init(&st);
    ch = nb_view_cycle_scale(&st, label, sizeof(label));
    CHECK(st.cur.scale == NB_SCALE_STRETCH && ch == NB_VIEW_CH_LAYOUT &&
          !strcmp(label, "Scale: Stretch"), "scale cycle 1 (%s)", label);
    nb_view_cycle_scale(&st, label, sizeof(label));
    CHECK(st.cur.scale == NB_SCALE_INTEGER, "scale cycle 2");
    nb_view_cycle_scale(&st, label, sizeof(label));
    CHECK(st.cur.scale == NB_SCALE_NONE, "scale cycle 3");
    nb_view_cycle_scale(&st, label, sizeof(label));
    CHECK(st.cur.scale == NB_SCALE_ASPECT, "scale cycle wraps");

    /* Area: no custom yet, so it is skipped. */
    for (i = 0; i < 4; i++) {
        ch = nb_view_cycle_area(&st, label, sizeof(label));
        CHECK(ch & NB_VIEW_CH_RES, "native res follows the area");
    }
    CHECK(st.cur.area == NB_AREA_4_3, "area 4 steps");
    nb_view_cycle_area(&st, label, sizeof(label));
    CHECK(st.cur.area == NB_AREA_FULL, "custom skipped when unset");

    /* Res: Native -> 4 presets -> (no custom) -> Native */
    ch = nb_view_cycle_res(&st, label, sizeof(label));
    CHECK(ch == NB_VIEW_CH_RES && st.cur.res_w == 2560, "res 1");
    for (i = 0; i < 3; i++) {
        nb_view_cycle_res(&st, label, sizeof(label));
    }
    CHECK(st.cur.res_w == 1280 && st.cur.res_h == 960, "res last preset");
    nb_view_cycle_res(&st, label, sizeof(label));
    CHECK(st.cur.res_w == 0, "res back to native");
    nb_vstore_note_res(&st, 1720, 1080);
    for (i = 0; i < 5; i++) {
        nb_view_cycle_res(&st, label, sizeof(label));
    }
    CHECK(st.cur.res_w == 1720 && !strcmp(label,
          "Guest resolution: 1720x1080"), "res custom (%s)", label);

    /* Nudge: a preset becomes custom where it is, then moves. */
    nb_vstore_init(&st);
    st.cur.area = NB_AREA_4_3;
    ch = nb_view_nudge(&st, 1920, 1080, 120, 8, 0, 0, label, sizeof(label));
    CHECK(st.cur.area == NB_AREA_CUSTOM && st.cur.cw == 1440 &&
          st.cur.ch == 1080 && st.cur.cx == 248 && st.cur.cy == 0 &&
          !st.cur.ccenter, "nudge right (%d,%d %dx%d)", st.cur.cx, st.cur.cy,
          st.cur.cw, st.cur.ch);
    nb_view_nudge(&st, 1920, 1080, 120, 10000, 0, 0, label, sizeof(label));
    CHECK(st.cur.cx == 480, "nudge clamps at the edge (%d)", st.cur.cx);
    nb_vstore_init(&st);
    ch = nb_view_nudge(&st, 1920, 1080, 120, 0, 0, -192, label, sizeof(label));
    CHECK(st.cur.cw == 1728 && st.cur.ch == 972 && st.cur.ccenter,
          "shrink keeps aspect and centre (%dx%d c%d)", st.cur.cw, st.cur.ch,
          st.cur.ccenter);
    CHECK(ch & NB_VIEW_CH_RES, "shrinking a native area re-hints");
    ch = nb_view_reset(&st, label, sizeof(label));
    CHECK(st.cur.area == NB_AREA_FULL && st.cur.scale == NB_SCALE_ASPECT,
          "reset");
}

static void test_override(void)
{
    struct nb_view a, b;

    nb_view_defaults(&a);
    nb_view_defaults(&b);
    a.scale = NB_SCALE_STRETCH;
    a.res_w = 1280;
    a.res_h = 960;
    b.scale = NB_SCALE_INTEGER;
    b.filter = NB_FILTER_NEAREST;
    nb_view_override(&a, &b, NB_VIEW_SET_SCALE);
    CHECK(a.scale == NB_SCALE_INTEGER && a.filter == NB_FILTER_LINEAR &&
          a.res_w == 1280, "override only what was given");
}

static void test_rules(void)
{
    struct nb_vstore st, back;
    char label[96], buf[8192];
    unsigned ch;

    nb_vstore_init(&st);
    st.cur.scale = NB_SCALE_ASPECT;
    st.cur.area = NB_AREA_FULL;
    /* No rule: a mode change changes nothing. */
    CHECK(nb_vstore_guest_mode(&st, 1280, 960, label, sizeof(label)) == 0,
          "no rule, no change");
    CHECK(!st.ruled, "not ruled");

    /* In 1280x960 the user picks 4:3 stretch and remembers it. */
    st.cur.area = NB_AREA_4_3;
    st.cur.scale = NB_SCALE_STRETCH;
    CHECK(nb_vstore_rule_save(&st, 1280, 960) == 0, "rule saved");
    CHECK(st.ruled && st.rule_w == 1280, "the rule is in force");
    /* The layout from before the edit (on arrival at 1280x960) is the one
     * to come back to. */
    CHECK(st.base.area == NB_AREA_FULL && st.base.scale == NB_SCALE_ASPECT,
          "base is the arrival layout");

    /* Back to native (no rule): the usual layout comes back. */
    ch = nb_vstore_guest_mode(&st, 5120, 1440, label, sizeof(label));
    CHECK(ch == NB_VIEW_CH_LAYOUT, "leaving the ruled mode re-lays out");
    CHECK(st.cur.area == NB_AREA_FULL && st.cur.scale == NB_SCALE_ASPECT,
          "usual layout back");
    CHECK(!st.ruled, "no rule in force");

    /* The guest goes to 1280x960 again: the rule applies by itself. */
    ch = nb_vstore_guest_mode(&st, 1280, 960, label, sizeof(label));
    CHECK(ch == NB_VIEW_CH_LAYOUT, "rule applied");
    CHECK(st.cur.area == NB_AREA_4_3 && st.cur.scale == NB_SCALE_STRETCH,
          "rule layout");
    CHECK(strstr(label, "1280x960") != NULL, "label names the mode: %s", label);
    CHECK(nb_vstore_guest_mode(&st, 1280, 960, label, sizeof(label)) == 0,
          "same mode again: nothing");

    /* A rule for native too: full, 1:1. */
    nb_vstore_guest_mode(&st, 5120, 1440, NULL, 0);
    st.cur.scale = NB_SCALE_NONE;
    CHECK(nb_vstore_rule_save(&st, 5120, 1440) == 1, "second rule");
    ch = nb_vstore_guest_mode(&st, 1280, 960, NULL, 0);
    CHECK(ch == NB_VIEW_CH_LAYOUT && st.cur.scale == NB_SCALE_STRETCH,
          "rule to rule");
    ch = nb_vstore_guest_mode(&st, 5120, 1440, NULL, 0);
    CHECK(ch == NB_VIEW_CH_LAYOUT && st.cur.scale == NB_SCALE_NONE,
          "native's own rule");

    /* The res setting is never touched by a rule. */
    st.cur.res_w = 1280;
    st.cur.res_h = 960;
    nb_vstore_guest_mode(&st, 1280, 960, NULL, 0);
    CHECK(st.cur.res_w == 1280 && st.cur.res_h == 960, "res kept");

    /* Saved and read back: rules survive, and the file keeps the base
     * layout, not the one a rule put in force. */
    nb_vstore_format(&st, buf, sizeof(buf));
    CHECK(strstr(buf, "rule=1280x960|") != NULL, "rule line: %s", buf);
    CHECK(strstr(buf, "rule=5120x1440|") != NULL, "native rule line");
    nb_vstore_init(&back);
    nb_vstore_parse(&back, buf);
    CHECK(back.nrule == 2, "two rules back (%d)", back.nrule);
    CHECK(back.rule[0].v.area == NB_AREA_4_3 &&
          back.rule[0].v.scale == NB_SCALE_STRETCH, "rule layout back");
    CHECK(back.cur.scale == st.base.scale && back.cur.area == st.base.area,
          "file has the base layout");
    CHECK(!back.ruled, "a loaded store starts with no rule in force");

    /* Delete: the one in force keeps its layout until the next change. */
    nb_vstore_rule_delete(&st, nb_vstore_rule_find(&st, 1280, 960));
    CHECK(st.nrule == 1 && nb_vstore_rule_find(&st, 1280, 960) < 0, "deleted");
    CHECK(st.cur.scale == NB_SCALE_STRETCH, "layout kept for now");

    /* Malformed lines are skipped; duplicates too. */
    nb_vstore_init(&back);
    nb_vstore_parse(&back, "rule=junk|scale=fit\nrule=800x600\n"
                           "rule=800x600|scale=stretch\nrule=800x600|scale=none\n");
    CHECK(back.nrule == 1 && back.rule[0].v.scale == NB_SCALE_STRETCH,
          "one good rule");
}

static void test_modes(void)
{
    struct nb_modes_rx rx;
    struct nb_vstore st;
    char label[96];
    unsigned i;

    memset(&rx, 0, sizeof(rx));
    /* A three-mode list. */
    CHECK(nb_modes_feed(&rx, 5120, 1440, 0, 3, 240000, 7, NB_MODE_NATIVE) == 0,
          "first");
    CHECK(nb_modes_feed(&rx, 1920, 1080, 1, 3, 240000, 7, 0) == 0, "second");
    CHECK(rx.cur.n == 0, "not shown before it is complete");
    CHECK(nb_modes_feed(&rx, 1280, 960, 2, 3, 240000, 7, NB_MODE_CUSTOM) == 1,
          "complete");
    CHECK(rx.cur.n == 3 && rx.cur.gen == 7 && rx.cur.mhz == 240000, "list");
    CHECK(rx.cur.flags[2] == NB_MODE_CUSTOM, "custom flag");
    CHECK(nb_modes_find(&rx.cur, 1920, 1080) == 1, "find");
    CHECK(nb_modes_find(&rx.cur, 800, 600) == -1, "not found");

    /* A list cut short and a wrong order are dropped; the old one stays. */
    CHECK(nb_modes_feed(&rx, 5120, 1440, 0, 2, 240000, 8, NB_MODE_NATIVE) == 0,
          "new list starts");
    CHECK(nb_modes_feed(&rx, 1280, 720, 1, 2, 240000, 9, 0) == -1,
          "mixed generations");
    CHECK(nb_modes_feed(&rx, 1280, 720, 1, 2, 240000, 8, 0) == -1,
          "no record 0 since");
    CHECK(rx.cur.n == 3 && rx.cur.gen == 7, "old list kept");
    CHECK(nb_modes_feed(&rx, 0, 1440, 0, 2, 1, 9, NB_MODE_NATIVE) == -1, "0 width");
    CHECK(nb_modes_feed(&rx, 640, 480, 0, NB_MODES_MAX + 1, 1, 9,
                        NB_MODE_NATIVE) == -1, "count over the cap");
    CHECK(nb_modes_feed(&rx, 640, 480, 0, 2, 1, 9, 0) == -1,
          "record 0 must be native");
    /* And a good one after all that. */
    CHECK(nb_modes_feed(&rx, 2560, 1440, 0, 1, 144000, 10, NB_MODE_NATIVE) == 1,
          "a one-mode list");
    CHECK(rx.cur.n == 1 && rx.cur.w[0] == 2560, "replaced");

    /* Ctrl+Alt+R walks the list from the mode the guest runs at. */
    memset(&rx, 0, sizeof(rx));
    nb_modes_feed(&rx, 5120, 1440, 0, 3, 240000, 1, NB_MODE_NATIVE);
    nb_modes_feed(&rx, 1920, 1080, 1, 3, 240000, 1, 0);
    nb_modes_feed(&rx, 1280, 960, 2, 3, 240000, 1, 0);
    nb_vstore_init(&st);
    CHECK(nb_view_cycle_mode(&st, &rx.cur, 5120, 1440, label, sizeof(label)) ==
          NB_VIEW_CH_RES && st.cur.res_w == 1920, "native -> next");
    nb_view_cycle_mode(&st, &rx.cur, 1280, 960, label, sizeof(label));
    CHECK(st.cur.res_w == 5120 && strstr(label, "native"), "wraps to native");
    nb_view_cycle_mode(&st, &rx.cur, 999, 999, label, sizeof(label));
    CHECK(st.cur.res_w == 5120, "unknown current: first");
    /* Without a list: the old presets. */
    struct nb_modes none = { 0 };
    st.cur.res_w = st.cur.res_h = 0;
    nb_view_cycle_mode(&st, &none, 0, 0, label, sizeof(label));
    CHECK(st.cur.res_w == nb_res_preset[0][0], "presets without a list");
    for (i = 0; i < 3; i++) {
        CHECK(rx.cur.w[i] != 0, "list intact");
    }
}

int main(void)
{
    test_fit();
    test_stretch();
    test_integer();
    test_none();
    test_areas();
    test_custom();
    test_clamp();
    test_fractional();
    test_guest_mode();
    test_names();
    test_store();
    test_cycles();
    test_override();
    test_rules();
    test_modes();
    printf("test_view: %d checks, %d failed\n", checks, fails);
    return fails ? 1 : 0;
}
