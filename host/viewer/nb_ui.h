/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * nb_ui.h -- the viewer's own UI layer: the settings menu (Ctrl+Alt+M), the
 * short on-screen notice a hotkey shows, and the small Conduit button that
 * fades in at the top edge.
 *
 * Display-server free.  Everything here paints into plain premultiplied
 * ARGB8888 memory and answers questions in logical window coordinates; the
 * Wayland backend shows the result on wl_subsurfaces of its own surface, the
 * X11 backend on child windows.  So layout, hit-testing and the input state
 * machine are all checked by test/test_ui.c without a compositor.
 *
 * Text is drawn with FreeType (the face found by fontconfig) when the viewer
 * was built with both; otherwise with the 5x7 bitmap font scaled up, so the
 * UI never depends on a font being installed.
 *
 * Nothing in here is on the frame path.  The menu paints when its state
 * changes, never per guest frame.
 */
#ifndef NB_UI_H
#define NB_UI_H

#include <stdbool.h>
#include <stdint.h>

#include "nb_view.h"

/* What a call asks the backend to do next (bits). */
#define NB_UI_VIEW      (1u << 0)   /* st->cur changed: re-place the picture */
#define NB_UI_RES       (1u << 1)   /* guest resolution changed: re-hint     */
#define NB_UI_SAVE      (1u << 2)   /* persist the store                     */
#define NB_UI_REDRAW    (1u << 3)   /* repaint the panel                     */
#define NB_UI_CLOSE     (1u << 4)   /* the menu closed itself                */
#define NB_UI_STATS     (1u << 5)   /* toggle the stats overlay              */
#define NB_UI_FULLSCREEN (1u << 6)  /* toggle fullscreen                     */
#define NB_UI_VM_SHUTDOWN (1u << 7) /* run the VM shutdown command           */
#define NB_UI_VM_REBOOT (1u << 8)   /* run the VM restart command            */
#define NB_UI_CURSOR    (1u << 9)   /* the wanted pointer shape changed      */
#define NB_UI_NOTICE    (1u << 10)  /* show ui->notice as an on-screen notice */

/* Pointer shapes the area editor asks for. */
enum {
    NB_UI_CUR_ARROW = 0,
    NB_UI_CUR_MOVE,
    NB_UI_CUR_N, NB_UI_CUR_S, NB_UI_CUR_W, NB_UI_CUR_E,
    NB_UI_CUR_NW, NB_UI_CUR_NE, NB_UI_CUR_SW, NB_UI_CUR_SE,
    NB_UI_CUR_TEXT,
    NB_UI_CUR_COUNT
};

/* What the menu needs to know about the world; the backend fills it in. */
struct nb_ui_env {
    int      win_w, win_h;      /* window content size, window coordinates  */
    unsigned s120;              /* output scale * 120 (120 = 1.0)           */
    int      buf_w, buf_h;      /* guest picture, 0 when there is none      */
    bool     stats_on;          /* the stats overlay is shown               */
    bool     fullscreen;
    bool     nearest_ok;        /* this backend can honour the sharp filter */
    bool     vm_actions;        /* shutdown/restart commands are available  */
    bool     translucent;       /* the panel layer can blend (Wayland: yes) */
};

#define NB_UI_TEXT_MAX 40
#define NB_UI_MAX_ITEMS 96

/* What a panel item is (nb_ui_item.id); arg says which one of a kind. */
enum {
    NB_UI_ID_NONE = 0,
    NB_UI_ID_CLOSE,             /* the x in the header                       */
    NB_UI_ID_RES_NATIVE,
    NB_UI_ID_RES_PRESET,        /* arg: index into nb_res_preset             */
    NB_UI_ID_RES_RECENT,        /* arg: index into st->recent                */
    NB_UI_ID_RES_FIELD,         /* custom WxH text field                     */
    NB_UI_ID_AREA,              /* arg: NB_AREA_*                            */
    NB_UI_ID_AREA_RESET,
    NB_UI_ID_SCALE,             /* arg: NB_SCALE_*                           */
    NB_UI_ID_FILTER,            /* arg: NB_FILTER_*                          */
    NB_UI_ID_PROF_LOAD,         /* arg: profile index                        */
    NB_UI_ID_PROF_DEL,          /* arg: profile index                        */
    NB_UI_ID_PROF_FIELD,        /* "save as" text field                      */
    NB_UI_ID_STATS,
    NB_UI_ID_FULLSCREEN,
    NB_UI_ID_VM_REBOOT,
    NB_UI_ID_VM_SHUTDOWN,
};

/* One clickable thing on the panel, panel CONTENT coordinates (logical; the
 * visible panel shows content y in [scroll, scroll + lh)). */
struct nb_ui_item {
    struct nb_rect r;
    int            id;          /* NB_UI_ID_*                                */
    int            arg;
    bool           disabled;    /* drawn, never acts                         */
};

struct nb_ui {
    struct nb_vstore *st;
    bool   open;

    /* panel */
    struct nb_ui_item item[NB_UI_MAX_ITEMS];
    int    nitem;
    int    lw, lh;              /* panel size, logical                       */
    int    content_h;           /* full content height (lh when no scrolling) */
    int    hover, press;        /* item index, -1 none                       */
    int    confirm;             /* item id awaiting a confirming click, 0 none */
    int    focus;               /* text field id with keyboard focus, 0 none */
    char   text[NB_UI_TEXT_MAX];/* the focused field's contents              */
    int    scroll;              /* logical px the profile list is scrolled   */

    /* area editor (pointer over the picture while the menu is open) */
    int    drag;                /* NB_UI_CUR_* being dragged, -1 none        */
    int    hot;                 /* NB_UI_CUR_* under the pointer             */
    double px, py;              /* last pointer, window coordinates          */
    double drag_x0, drag_y0;    /* where the drag began                      */
    struct nb_rect drag_r0;     /* the area then, output pixels              */

    char   notice[64];          /* text for NB_UI_NOTICE                     */
};

void nb_ui_init(struct nb_ui *ui, struct nb_vstore *st);

/* Open or close.  Returns NB_UI_* bits (REDRAW, and CLOSE on a close). */
unsigned nb_ui_set_open(struct nb_ui *ui, const struct nb_ui_env *env,
                        bool open);

/* Lay the panel out for env (call before painting or hit-testing when env
 * changed).  Logical size in lw, lh. */
void nb_ui_layout(struct nb_ui *ui, const struct nb_ui_env *env, int *lw,
                  int *lh);
/* Index of the item with this id/arg after the last layout, or -1. */
int nb_ui_find(const struct nb_ui *ui, int id, int arg);
/* Tests: draw text with the built-in bitmap font even when FreeType is
 * available. */
void nb_ui_force_bitmap_font(bool on);

/* Where the panel sits in the window, window coordinates. */
void nb_ui_panel_rect(const struct nb_ui *ui, const struct nb_ui_env *env,
                      struct nb_rect *r);

/* Pointer over the PANEL, panel-local logical coordinates. */
unsigned nb_ui_panel_motion(struct nb_ui *ui, const struct nb_ui_env *env,
                            double x, double y);
unsigned nb_ui_panel_button(struct nb_ui *ui, const struct nb_ui_env *env,
                            bool down);
unsigned nb_ui_panel_wheel(struct nb_ui *ui, const struct nb_ui_env *env,
                           int steps);  /* + = down */
unsigned nb_ui_panel_leave(struct nb_ui *ui);

/* Pointer ANYWHERE ELSE in the window while the menu is open, window
 * coordinates: drags the output area's edges, corners or body. */
unsigned nb_ui_area_motion(struct nb_ui *ui, const struct nb_ui_env *env,
                           double wx, double wy);
unsigned nb_ui_area_button(struct nb_ui *ui, const struct nb_ui_env *env,
                           bool down);
int      nb_ui_area_cursor(const struct nb_ui *ui);   /* NB_UI_CUR_* */
/* Which handle (NB_UI_CUR_*) of area `a` is at (wx, wy); ARROW = none. */
int      nb_ui_area_hit(const struct nb_rect *a, double wx, double wy);

/* A key while the menu is open (evdev code).  Esc closes; a focused text
 * field takes digits, 'x', letters (profile names), Backspace, Enter. */
unsigned nb_ui_key(struct nb_ui *ui, const struct nb_ui_env *env,
                   unsigned code, bool shift, bool down);

/* Paint the panel into px (pw x ph PHYSICAL pixels = logical * s120/120,
 * rounded up).  Opaque when !env->translucent (X11 without a compositor). */
void nb_ui_paint_panel(struct nb_ui *ui, const struct nb_ui_env *env,
                       uint32_t *px, int pw, int ph, int stride_px);

/* The notice: logical size for `txt`, and its pixels. */
void nb_ui_notice_size(const char *txt, int *lw, int *lh);
void nb_ui_paint_notice(const char *txt, unsigned s120, uint32_t *px, int pw,
                        int ph, int stride_px, bool opaque);

/* The top-edge Conduit button. */
void nb_ui_button_size(int *lw, int *lh);
void nb_ui_paint_button(unsigned s120, uint32_t *px, int pw, int ph,
                        int stride_px, double alpha, bool hot, bool opaque);
/* Show the button when the pointer is within this many logical px of the
 * window's top edge. */
#define NB_UI_BUTTON_ZONE 6

/* Physical size of a logical extent at s120 (rounded up). */
static inline int nb_ui_phys(int l, unsigned s120)
{
    return (int)(((long)l * (s120 ? s120 : 120) + 119) / 120);
}

#endif /* NB_UI_H */
