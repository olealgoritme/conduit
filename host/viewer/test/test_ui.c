/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * test_ui.c -- the viewer menu without a compositor: layout, hit-testing, the
 * input state machine, the area editor's drag arithmetic and painting bounds.
 */
#include <linux/input-event-codes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "../nb_ui.h"

static int fails, checks;
#define CHECK(c, ...) do { checks++; if (!(c)) { fails++; \
    printf("FAIL %s:%d: ", __FILE__, __LINE__); printf(__VA_ARGS__); \
    printf("\n"); } } while (0)

static struct nb_ui_env env_of(int w, int h, unsigned s120)
{
    struct nb_ui_env e;

    memset(&e, 0, sizeof(e));
    e.win_w = w;
    e.win_h = h;
    e.s120 = s120;
    e.buf_w = 1920;
    e.buf_h = 1080;
    e.nearest_ok = true;
    e.vm_actions = true;
    e.translucent = true;
    return e;
}

/* Click item idx: scroll it into view, move over it, press, release. */
static unsigned click_idx(struct nb_ui *ui, const struct nb_ui_env *e, int idx)
{
    const struct nb_rect *r;
    unsigned bits = 0;

    if (idx < 0) {
        return 0;
    }
    r = &ui->item[idx].r;
    if (r->y < ui->scroll || r->y + r->h > ui->scroll + ui->lh) {
        ui->scroll = r->y;
        nb_ui_layout(ui, e, NULL, NULL);
    }
    r = &ui->item[idx].r;
    bits |= nb_ui_panel_motion(ui, e, r->x + r->w / 2.0,
                               r->y + r->h / 2.0 - ui->scroll);
    bits |= nb_ui_panel_button(ui, e, true);
    bits |= nb_ui_panel_button(ui, e, false);
    return bits;
}

static unsigned click(struct nb_ui *ui, const struct nb_ui_env *e, int id,
                      int arg)
{
    int idx;

    nb_ui_layout(ui, e, NULL, NULL);
    idx = nb_ui_find(ui, id, arg);
    CHECK(idx >= 0, "item %d/%d present", id, arg);
    return click_idx(ui, e, idx);
}

static unsigned type(struct nb_ui *ui, const struct nb_ui_env *e,
                     const unsigned *codes, int n, bool shift)
{
    unsigned bits = 0;
    int i;

    for (i = 0; i < n; i++) {
        bits |= nb_ui_key(ui, e, codes[i], shift, true);
        bits |= nb_ui_key(ui, e, codes[i], shift, false);
    }
    return bits;
}

static void test_area_hit(void)
{
    struct nb_rect a = { 100, 100, 400, 300 };

    CHECK(nb_ui_area_hit(&a, 300, 250) == NB_UI_CUR_MOVE, "centre");
    CHECK(nb_ui_area_hit(&a, 101, 101) == NB_UI_CUR_NW, "nw");
    CHECK(nb_ui_area_hit(&a, 498, 101) == NB_UI_CUR_NE, "ne");
    CHECK(nb_ui_area_hit(&a, 101, 398) == NB_UI_CUR_SW, "sw");
    CHECK(nb_ui_area_hit(&a, 498, 398) == NB_UI_CUR_SE, "se");
    CHECK(nb_ui_area_hit(&a, 300, 103) == NB_UI_CUR_N, "n");
    CHECK(nb_ui_area_hit(&a, 300, 396) == NB_UI_CUR_S, "s");
    CHECK(nb_ui_area_hit(&a, 104, 250) == NB_UI_CUR_W, "w");
    CHECK(nb_ui_area_hit(&a, 497, 250) == NB_UI_CUR_E, "e");
    /* the band reaches a little outside, too */
    CHECK(nb_ui_area_hit(&a, 95, 250) == NB_UI_CUR_W, "w outside");
    CHECK(nb_ui_area_hit(&a, 300, 405) == NB_UI_CUR_S, "s outside");
    CHECK(nb_ui_area_hit(&a, 50, 250) == NB_UI_CUR_ARROW, "far left");
    CHECK(nb_ui_area_hit(&a, 300, 420) == NB_UI_CUR_ARROW, "far below");
    CHECK(nb_ui_area_hit(&a, 0, 0) == NB_UI_CUR_ARROW, "origin");
}

static void test_layout(void)
{
    static const int sizes[][2] = {
        { 800, 600 }, { 1280, 720 }, { 1920, 1080 }, { 2560, 1440 },
        { 5120, 1440 }, { 1707, 960 }, { 640, 400 }, { 300, 500 },
    };
    static const unsigned scales[] = { 120, 150, 180, 240 };
    struct nb_vstore st;
    struct nb_ui ui;
    size_t i, j;
    int a, b2;

    nb_vstore_init(&st);
    nb_vstore_note_res(&st, 1366, 768);
    nb_vstore_note_res(&st, 3440, 1440);
    st.cur.cw = 1000;
    st.cur.ch = 700;
    nb_vstore_profile_save(&st, "CS2 4:3 stretched");
    nb_vstore_profile_save(&st, "Desktop");
    nb_ui_init(&ui, &st);
    for (i = 0; i < sizeof(sizes) / sizeof(sizes[0]); i++) {
        for (j = 0; j < sizeof(scales) / sizeof(scales[0]); j++) {
            struct nb_ui_env e = env_of(sizes[i][0], sizes[i][1], scales[j]);
            struct nb_rect pr;
            int lw, lh;

            nb_ui_set_open(&ui, &e, true);
            nb_ui_layout(&ui, &e, &lw, &lh);
            nb_ui_panel_rect(&ui, &e, &pr);
            CHECK(lw == pr.w && lh == pr.h, "rect = size");
            if (sizes[i][0] >= 240) {
                CHECK(pr.x >= 0 && pr.x + pr.w <= e.win_w, "%dx%d: x %d w %d",
                      e.win_w, e.win_h, pr.x, pr.w);
            }
            CHECK(pr.y >= 0 && (pr.y + pr.h <= e.win_h || e.win_h < 152),
                  "%dx%d: y %d h %d", e.win_w, e.win_h, pr.y, pr.h);
            CHECK(lh <= ui.content_h, "lh <= content");
            if (e.win_h - 32 >= ui.content_h) {
                CHECK(lh == ui.content_h, "no scroll needed");
            }
            CHECK(ui.nitem > 20, "items %d", ui.nitem);
            for (a = 0; a < ui.nitem; a++) {
                const struct nb_rect *r = &ui.item[a].r;

                CHECK(r->x >= 0 && r->x + r->w <= lw && r->y >= 0 &&
                      r->y + r->h <= ui.content_h && r->w > 0 && r->h > 0,
                      "%dx%d item %d (id %d) %d,%d %dx%d in %dx%d",
                      e.win_w, e.win_h, a, ui.item[a].id, r->x, r->y, r->w,
                      r->h, lw, ui.content_h);
                for (b2 = a + 1; b2 < ui.nitem; b2++) {
                    const struct nb_rect *q = &ui.item[b2].r;
                    bool ov = r->x < q->x + q->w && q->x < r->x + r->w &&
                              r->y < q->y + q->h && q->y < r->y + r->h;

                    CHECK(!ov, "overlap %d/%d (ids %d %d)", a, b2,
                          ui.item[a].id, ui.item[b2].id);
                }
            }
            /* each section starts below the previous one's last row, with
             * room for its header */
            {
                static const int seq[] = {
                    NB_UI_ID_RES_FIELD, NB_UI_ID_AREA, NB_UI_ID_SCALE,
                    NB_UI_ID_FILTER, NB_UI_ID_PROF_LOAD, NB_UI_ID_STATS,
                    NB_UI_ID_VM_REBOOT,
                };
                size_t q;

                for (q = 0; q + 1 < sizeof(seq) / sizeof(seq[0]); q++) {
                    int lo = 1 << 30, hi = 0;

                    for (a = 0; a < ui.nitem; a++) {
                        const struct nb_rect *r = &ui.item[a].r;

                        if (ui.item[a].id == seq[q] && r->y + r->h > hi) {
                            hi = r->y + r->h;
                        }
                        if (ui.item[a].id == seq[q + 1] && r->y < lo) {
                            lo = r->y;
                        }
                    }
                    CHECK(lo >= hi + 14, "section %d starts at %d, previous "
                          "ends at %d", seq[q + 1], lo, hi);
                }
            }
            /* right-aligned with a margin when there is room */
            if (e.win_w >= 360 + 32) {
                CHECK(pr.x + pr.w == e.win_w - 16, "right margin");
            }
        }
    }
    /* VM section only when available */
    {
        struct nb_ui_env e = env_of(1920, 1080, 120);

        e.vm_actions = false;
        nb_ui_layout(&ui, &e, NULL, NULL);
        CHECK(nb_ui_find(&ui, NB_UI_ID_VM_SHUTDOWN, 0) < 0, "no vm items");
        e.vm_actions = true;
        nb_ui_layout(&ui, &e, NULL, NULL);
        CHECK(nb_ui_find(&ui, NB_UI_ID_VM_SHUTDOWN, 0) >= 0, "vm items");
        CHECK(nb_ui_find(&ui, NB_UI_ID_RES_RECENT, 1) >= 0, "recent chips");
        CHECK(nb_ui_find(&ui, NB_UI_ID_AREA, NB_AREA_CUSTOM) >= 0,
              "custom area chip once a custom size exists");
        CHECK(nb_ui_find(&ui, NB_UI_ID_PROF_DEL, 1) >= 0, "profile rows");
    }
    /* scrolling in a short window */
    {
        struct nb_ui_env e = env_of(800, 400, 120);
        unsigned bits;

        ui.scroll = 0;
        nb_ui_layout(&ui, &e, NULL, NULL);
        CHECK(ui.content_h > ui.lh, "scrolls");
        bits = nb_ui_panel_wheel(&ui, &e, 1);
        CHECK(ui.scroll == 40 && (bits & NB_UI_REDRAW), "wheel %d", ui.scroll);
        nb_ui_panel_wheel(&ui, &e, 1000);
        CHECK(ui.scroll == ui.content_h - ui.lh, "clamped at the end");
        nb_ui_panel_wheel(&ui, &e, -1000);
        CHECK(ui.scroll == 0, "clamped at the top");
    }
}

static void test_clicks(void)
{
    struct nb_vstore st;
    struct nb_ui ui;
    struct nb_ui_env e = env_of(1920, 1080, 120);
    unsigned bits;

    nb_vstore_init(&st);
    nb_ui_init(&ui, &st);
    nb_ui_set_open(&ui, &e, true);

    bits = click(&ui, &e, NB_UI_ID_SCALE, NB_SCALE_STRETCH);
    CHECK(st.cur.scale == NB_SCALE_STRETCH, "stretch");
    CHECK((bits & (NB_UI_VIEW | NB_UI_SAVE | NB_UI_REDRAW | NB_UI_NOTICE)) ==
          (NB_UI_VIEW | NB_UI_SAVE | NB_UI_REDRAW | NB_UI_NOTICE) &&
          !(bits & NB_UI_RES), "scale bits %#x", bits);
    CHECK(strstr(ui.notice, "Stretch") != NULL, "notice '%s'", ui.notice);

    bits = click(&ui, &e, NB_UI_ID_SCALE, NB_SCALE_STRETCH);
    CHECK(!(bits & (NB_UI_SAVE | NB_UI_VIEW)), "same again: nothing %#x", bits);

    bits = click(&ui, &e, NB_UI_ID_AREA, NB_AREA_16_9);
    CHECK(st.cur.area == NB_AREA_16_9, "16:9");
    CHECK((bits & NB_UI_VIEW) && (bits & NB_UI_RES),
          "area at native re-hints %#x", bits);

    bits = click(&ui, &e, NB_UI_ID_RES_PRESET, 1);
    CHECK(st.cur.res_w == nb_res_preset[1][0] &&
          st.cur.res_h == nb_res_preset[1][1], "preset");
    CHECK((bits & NB_UI_RES) && !(bits & NB_UI_VIEW) && (bits & NB_UI_SAVE),
          "res bits %#x", bits);

    bits = click(&ui, &e, NB_UI_ID_AREA, NB_AREA_4_3);
    CHECK((bits & NB_UI_VIEW) && !(bits & NB_UI_RES),
          "area with a fixed res does not re-hint %#x", bits);

    bits = click(&ui, &e, NB_UI_ID_AREA_RESET, 0);
    CHECK(st.cur.area == NB_AREA_FULL && (bits & NB_UI_VIEW), "reset area");

    bits = click(&ui, &e, NB_UI_ID_RES_NATIVE, 0);
    CHECK(!st.cur.res_w && !st.cur.res_h && (bits & NB_UI_RES), "native");

    bits = click(&ui, &e, NB_UI_ID_FILTER, NB_FILTER_NEAREST);
    CHECK(st.cur.filter == NB_FILTER_NEAREST && (bits & NB_UI_VIEW), "sharp");
    e.nearest_ok = false;
    st.cur.filter = NB_FILTER_LINEAR;
    bits = click(&ui, &e, NB_UI_ID_FILTER, NB_FILTER_NEAREST);
    CHECK(st.cur.filter == NB_FILTER_LINEAR && !(bits & NB_UI_SAVE),
          "sharp disabled where unsupported");
    e.nearest_ok = true;

    bits = click(&ui, &e, NB_UI_ID_STATS, 0);
    CHECK(bits & NB_UI_STATS, "stats");
    bits = click(&ui, &e, NB_UI_ID_FULLSCREEN, 0);
    CHECK(bits & NB_UI_FULLSCREEN, "fullscreen");

    /* press on one item, release on another: nothing */
    {
        int a = nb_ui_find(&ui, NB_UI_ID_SCALE, NB_SCALE_INTEGER);
        int b = nb_ui_find(&ui, NB_UI_ID_SCALE, NB_SCALE_NONE);
        const struct nb_rect *ra = &ui.item[a].r, *rb = &ui.item[b].r;

        nb_ui_panel_motion(&ui, &e, ra->x + 2, ra->y + 2 - ui.scroll);
        nb_ui_panel_button(&ui, &e, true);
        nb_ui_panel_motion(&ui, &e, rb->x + 2, rb->y + 2 - ui.scroll);
        bits = nb_ui_panel_button(&ui, &e, false);
        CHECK(st.cur.scale == NB_SCALE_STRETCH && !(bits & NB_UI_VIEW),
              "drag-off cancels");
    }

    bits = click(&ui, &e, NB_UI_ID_CLOSE, 0);
    CHECK((bits & NB_UI_CLOSE) && !ui.open, "close");
}

static void test_text(void)
{
    static const unsigned res[] = { KEY_1, KEY_3, KEY_6, KEY_6, KEY_X, KEY_7,
                                    KEY_6, KEY_8 };
    static const unsigned preset[] = { KEY_1, KEY_9, KEY_2, KEY_0, KEY_X,
                                       KEY_1, KEY_0, KEY_8, KEY_0 };
    static const unsigned cs2[] = { KEY_C, KEY_S };
    static const unsigned name2[] = { KEY_SPACE, KEY_2 };
    struct nb_vstore st;
    struct nb_ui ui;
    struct nb_ui_env e = env_of(1920, 1080, 150);
    unsigned bits;

    nb_vstore_init(&st);
    nb_ui_init(&ui, &st);
    nb_ui_set_open(&ui, &e, true);

    click(&ui, &e, NB_UI_ID_RES_FIELD, 0);
    CHECK(ui.focus == NB_UI_ID_RES_FIELD, "focus");
    type(&ui, &e, preset, 9, false);
    CHECK(!strcmp(ui.text, "1920x1080"), "typed '%s'", ui.text);
    bits = nb_ui_key(&ui, &e, KEY_ENTER, false, true);
    CHECK(st.cur.res_w == 1920 && st.cur.res_h == 1080, "applied");
    CHECK((bits & (NB_UI_RES | NB_UI_SAVE | NB_UI_REDRAW | NB_UI_NOTICE)) ==
          (NB_UI_RES | NB_UI_SAVE | NB_UI_REDRAW | NB_UI_NOTICE),
          "enter bits %#x", bits);
    CHECK(ui.focus == 0, "unfocused after enter");
    CHECK(st.nrecent == 0, "a preset is not 'recent'");

    click(&ui, &e, NB_UI_ID_RES_FIELD, 0);
    CHECK(!strcmp(ui.text, "1920x1080"), "field starts with the current res");
    nb_ui_key(&ui, &e, KEY_BACKSPACE, false, true);
    for (int i = 0; i < 20; i++) {
        nb_ui_key(&ui, &e, KEY_BACKSPACE, false, true);
    }
    CHECK(ui.text[0] == '\0', "cleared");
    type(&ui, &e, cs2, 2, false);
    CHECK(ui.text[0] == '\0', "letters are not resolution characters");
    type(&ui, &e, res, 8, false);
    bits = nb_ui_key(&ui, &e, KEY_KPENTER, false, true);
    CHECK(st.cur.res_w == 1366 && st.cur.res_h == 768, "custom");
    CHECK(st.nrecent == 1 && st.recent[0][0] == 1366, "noted as recent");
    nb_ui_layout(&ui, &e, NULL, NULL);
    CHECK(nb_ui_find(&ui, NB_UI_ID_RES_RECENT, 0) >= 0, "recent chip");

    /* a bad one keeps focus and says so */
    click(&ui, &e, NB_UI_ID_RES_FIELD, 0);
    while (ui.text[0]) {
        nb_ui_key(&ui, &e, KEY_BACKSPACE, false, true);
    }
    nb_ui_key(&ui, &e, KEY_X, false, true);
    bits = nb_ui_key(&ui, &e, KEY_ENTER, false, true);
    CHECK((bits & NB_UI_NOTICE) && !(bits & NB_UI_RES) &&
          ui.focus == NB_UI_ID_RES_FIELD, "bad res %#x", bits);
    /* Esc: unfocus first, then close */
    bits = nb_ui_key(&ui, &e, KEY_ESC, false, true);
    CHECK(ui.open && !ui.focus && (bits & NB_UI_REDRAW), "esc unfocuses");
    bits = nb_ui_key(&ui, &e, KEY_ESC, false, true);
    CHECK(!ui.open && (bits & NB_UI_CLOSE), "esc closes");

    /* profiles */
    nb_ui_set_open(&ui, &e, true);
    st.cur.scale = NB_SCALE_STRETCH;
    st.cur.area = NB_AREA_4_3;
    click(&ui, &e, NB_UI_ID_PROF_FIELD, 0);
    type(&ui, &e, cs2, 2, true);
    type(&ui, &e, name2, 2, false);
    CHECK(!strcmp(ui.text, "CS 2"), "name '%s'", ui.text);
    bits = nb_ui_key(&ui, &e, KEY_ENTER, false, true);
    CHECK(st.nprof == 1 && !strcmp(st.prof[0].name, "CS 2") &&
          (bits & NB_UI_SAVE) && (bits & NB_UI_NOTICE), "saved");
    st.cur.scale = NB_SCALE_ASPECT;
    st.cur.area = NB_AREA_FULL;
    bits = click(&ui, &e, NB_UI_ID_PROF_LOAD, 0);
    CHECK(st.cur.scale == NB_SCALE_STRETCH && st.cur.area == NB_AREA_4_3 &&
          (bits & NB_UI_VIEW) && (bits & NB_UI_SAVE), "loaded %#x", bits);
    bits = click(&ui, &e, NB_UI_ID_PROF_DEL, 0);
    CHECK(st.nprof == 1 && ui.confirm != 0, "delete arms");
    bits = click(&ui, &e, NB_UI_ID_PROF_DEL, 0);
    CHECK(st.nprof == 0 && (bits & NB_UI_SAVE), "deleted");
    click(&ui, &e, NB_UI_ID_PROF_FIELD, 0);
    bits = nb_ui_key(&ui, &e, KEY_ENTER, false, true);
    CHECK(st.nprof == 0 && !(bits & NB_UI_SAVE), "empty name refused");
}

static void test_vm(void)
{
    struct nb_vstore st;
    struct nb_ui ui;
    struct nb_ui_env e = env_of(2560, 1440, 120);
    unsigned bits;

    nb_vstore_init(&st);
    nb_ui_init(&ui, &st);
    nb_ui_set_open(&ui, &e, true);
    bits = click(&ui, &e, NB_UI_ID_VM_SHUTDOWN, 0);
    CHECK(!(bits & NB_UI_VM_SHUTDOWN) && ui.confirm, "first click arms");
    bits = click(&ui, &e, NB_UI_ID_VM_SHUTDOWN, 0);
    CHECK(bits & NB_UI_VM_SHUTDOWN, "second click acts");
    CHECK(!ui.confirm, "disarmed");

    bits = click(&ui, &e, NB_UI_ID_VM_REBOOT, 0);
    CHECK(ui.confirm && !(bits & NB_UI_VM_REBOOT), "reboot arms");
    click(&ui, &e, NB_UI_ID_SCALE, NB_SCALE_INTEGER);
    CHECK(!ui.confirm, "another click cancels");
    bits = click(&ui, &e, NB_UI_ID_VM_REBOOT, 0);
    CHECK(!(bits & NB_UI_VM_REBOOT), "re-armed, not fired");
    bits = click(&ui, &e, NB_UI_ID_VM_SHUTDOWN, 0);
    CHECK(!(bits & (NB_UI_VM_REBOOT | NB_UI_VM_SHUTDOWN)),
          "arming one does not confirm the other");
    bits = click(&ui, &e, NB_UI_ID_VM_REBOOT, 0);
    bits = click(&ui, &e, NB_UI_ID_VM_REBOOT, 0);
    CHECK(bits & NB_UI_VM_REBOOT, "reboot after its own confirm");
    nb_ui_set_open(&ui, &e, false);
    CHECK(!ui.confirm && ui.drag < 0 && ui.hover < 0, "close resets");
}

static void test_drag(void)
{
    struct nb_vstore st;
    struct nb_ui ui;
    struct nb_ui_env e = env_of(1920, 1080, 120);
    unsigned bits;

    nb_vstore_init(&st);
    nb_ui_init(&ui, &st);
    nb_ui_set_open(&ui, &e, true);

    /* full window: drag the right edge 200 px in */
    bits = nb_ui_area_motion(&ui, &e, 1915, 500);
    CHECK(nb_ui_area_cursor(&ui) == NB_UI_CUR_E && (bits & NB_UI_CURSOR),
          "hot E");
    nb_ui_area_button(&ui, &e, true);
    bits = nb_ui_area_motion(&ui, &e, 1715, 500);
    CHECK(st.cur.area == NB_AREA_CUSTOM && st.cur.cw == 1720 &&
          st.cur.ch == 1080 && st.cur.cx == 0 && st.cur.cy == 0,
          "edge drag %dx%d+%d+%d", st.cur.cw, st.cur.ch, st.cur.cx, st.cur.cy);
    CHECK((bits & NB_UI_VIEW) && (bits & NB_UI_RES), "live, re-hints %#x",
          bits);
    CHECK(nb_ui_area_cursor(&ui) == NB_UI_CUR_E, "cursor held while dragging");
    bits = nb_ui_area_button(&ui, &e, false);
    CHECK((bits & NB_UI_SAVE) && (bits & NB_UI_NOTICE) && ui.drag < 0,
          "drop saves");
    CHECK(strstr(ui.notice, "1720x1080") != NULL, "notice '%s'", ui.notice);

    /* the minimum size holds */
    st.cur.area = NB_AREA_CUSTOM;
    st.cur.cw = 960;
    st.cur.ch = 540;
    st.cur.ccenter = true;
    nb_ui_area_motion(&ui, &e, 482, 500);       /* left edge at x=480 */
    CHECK(nb_ui_area_cursor(&ui) == NB_UI_CUR_W, "hot W %d",
          nb_ui_area_cursor(&ui));
    nb_ui_area_button(&ui, &e, true);
    nb_ui_area_motion(&ui, &e, 1700, 500);
    CHECK(st.cur.cw == 64 && st.cur.cx == 1440 - 64, "min width %d at %d",
          st.cur.cw, st.cur.cx);
    nb_ui_area_button(&ui, &e, false);

    /* move with centre snap */
    st.cur.cw = 960;
    st.cur.ch = 540;
    st.cur.ccenter = true;
    nb_ui_area_motion(&ui, &e, 960, 540);
    CHECK(nb_ui_area_cursor(&ui) == NB_UI_CUR_MOVE, "hot move");
    nb_ui_area_button(&ui, &e, true);
    nb_ui_area_motion(&ui, &e, 965, 543);
    CHECK(st.cur.cx == 480 && st.cur.cy == 270 && st.cur.ccenter,
          "snapped to centre %d,%d", st.cur.cx, st.cur.cy);
    nb_ui_area_motion(&ui, &e, 1160, 540);
    CHECK(st.cur.cx == 680 && st.cur.cy == 270 && !st.cur.ccenter,
          "moved %d,%d", st.cur.cx, st.cur.cy);
    nb_ui_area_motion(&ui, &e, 5000, 5000);
    CHECK(st.cur.cx == 960 && st.cur.cy == 540, "clamped inside %d,%d",
          st.cur.cx, st.cur.cy);
    bits = nb_ui_area_motion(&ui, &e, 5001, 5000);
    CHECK(!(bits & NB_UI_RES), "a move does not change the size");
    nb_ui_area_button(&ui, &e, false);

    /* fractional scale: window deltas are scaled to output pixels */
    {
        struct nb_ui_env f = env_of(1280, 720, 180);   /* 1920x1080 output */

        st.cur.area = NB_AREA_FULL;
        nb_ui_area_motion(&ui, &f, 640, 717);
        CHECK(nb_ui_area_cursor(&ui) == NB_UI_CUR_S, "hot S");
        nb_ui_area_button(&ui, &f, true);
        nb_ui_area_motion(&ui, &f, 640, 617);
        CHECK(st.cur.cw == 1920 && st.cur.ch == 1080 - 150,
              "1.5x: %dx%d", st.cur.cw, st.cur.ch);
        nb_ui_area_button(&ui, &f, false);
    }

    /* a click on the bars outside the area does nothing */
    st.cur.area = NB_AREA_4_3;
    nb_ui_area_motion(&ui, &e, 20, 500);
    CHECK(nb_ui_area_cursor(&ui) == NB_UI_CUR_ARROW, "bars");
    bits = nb_ui_area_button(&ui, &e, true);
    CHECK(ui.drag < 0 && !(bits & NB_UI_VIEW), "no drag on the bars");
}

#define GUARD 0xdeadbeefu

static int paint_one(const struct nb_ui_env *e, struct nb_ui *ui, int which)
{
    int lw, lh, pw, ph, stride, rows, x, y, nz = 0, bad = 0;
    uint32_t *buf;

    if (which == 0) {
        nb_ui_layout(ui, e, &lw, &lh);
    } else if (which == 1) {
        nb_ui_notice_size("Scale: Stretch", &lw, &lh);
    } else {
        nb_ui_button_size(&lw, &lh);
    }
    pw = nb_ui_phys(lw, e->s120);
    ph = nb_ui_phys(lh, e->s120);
    stride = pw + 7;
    rows = ph + 3;
    buf = malloc((size_t)stride * rows * 4);
    for (x = 0; x < stride * rows; x++) {
        buf[x] = GUARD;
    }
    if (which == 0) {
        nb_ui_paint_panel(ui, e, buf, pw, ph, stride);
    } else if (which == 1) {
        nb_ui_paint_notice("Scale: Stretch", e->s120, buf, pw, ph, stride,
                           !e->translucent);
    } else {
        nb_ui_paint_button(e->s120, buf, pw, ph, stride, 0.6, true,
                           !e->translucent);
    }
    for (y = 0; y < rows; y++) {
        for (x = 0; x < stride; x++) {
            uint32_t v = buf[(size_t)y * stride + x];
            bool in = x < pw && y < ph;

            if (!in && v != GUARD) {
                bad++;
            }
            if (in) {
                unsigned a = v >> 24;

                if (v) {
                    nz++;
                }
                /* premultiplied: no channel above alpha */
                if (((v >> 16) & 0xff) > a || ((v >> 8) & 0xff) > a ||
                    (v & 0xff) > a) {
                    bad++;
                }
                if (!e->translucent && which != 2 && a != 0xff) {
                    bad++;
                }
            }
        }
    }
    free(buf);
    CHECK(!bad, "paint %d at s120 %u: %d bad pixels", which, e->s120, bad);
    CHECK(nz > pw * ph / 4, "paint %d drew something (%d/%d)", which, nz,
          pw * ph);
    return nz;
}

static void test_paint(void)
{
    static const unsigned scales[] = { 120, 150, 180, 240 };
    struct nb_vstore st;
    struct nb_ui ui;
    size_t i;
    int pass, w;

    nb_vstore_init(&st);
    nb_vstore_note_res(&st, 1366, 768);
    nb_vstore_profile_save(&st, "CS2 4:3 stretched");
    nb_ui_init(&ui, &st);
    for (pass = 0; pass < 2; pass++) {
        nb_ui_force_bitmap_font(pass == 1);
        for (i = 0; i < sizeof(scales) / sizeof(scales[0]); i++) {
            struct nb_ui_env e = env_of(1920, 1080, scales[i]);

            nb_ui_set_open(&ui, &e, true);
            for (w = 0; w < 3; w++) {
                paint_one(&e, &ui, w);
            }
            e.translucent = false;
            for (w = 0; w < 3; w++) {
                paint_one(&e, &ui, w);
            }
            /* hover, focus and confirm states paint too */
            ui.hover = 3;
            ui.focus = NB_UI_ID_RES_FIELD;
            snprintf(ui.text, sizeof(ui.text), "1366x768");
            ui.confirm = NB_UI_ID_VM_SHUTDOWN << 8;
            paint_one(&e, &ui, 0);
            /* scrolled, in a short window */
            e = env_of(900, 400, scales[i]);
            nb_ui_layout(&ui, &e, NULL, NULL);
            ui.scroll = 100;
            paint_one(&e, &ui, 0);
            ui.scroll = 0;
        }
    }
    nb_ui_force_bitmap_font(false);
}

int main(void)
{
    test_area_hit();
    test_layout();
    test_clicks();
    test_text();
    test_vm();
    test_drag();
    test_paint();
    printf("test_ui: %d checks, %d failed\n", checks, fails);
    return fails ? 1 : 0;
}
