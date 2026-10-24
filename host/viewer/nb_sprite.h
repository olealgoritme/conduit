/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * nb_sprite.h -- when the viewer draws the guest cursor itself, and what it
 * must damage when that drawing changes.
 *
 * Under grab the host pointer is locked and shows nothing, so the viewer
 * draws the guest's cursor at the position the guest reports
 * (CMD_CURSOR_POS).  That drawing must go the moment any of its conditions
 * goes -- the grab released, the window losing focus or the pointer, the
 * guest hiding its cursor, the menu opening -- and the screen it covered
 * must be repainted: a compositor that repaints only what a commit damages
 * (Hyprland) otherwise keeps the cursor's last pixels on screen after its
 * surface is gone.  Pure, so test/test_sprite.c checks every rule without a
 * compositor.
 */
#ifndef NB_SPRITE_H
#define NB_SPRITE_H

#include <stdbool.h>

#include "nb_view.h"

struct nb_spr_in {
    bool grabbed;       /* CTRL+ALT+G is on                                  */
    bool focused;       /* the window has the keyboard                       */
    bool ptr_in;        /* the pointer is over the window's content          */
    bool shown;         /* the guest shows a cursor (CMD_CURSOR with image)  */
    bool image;         /* ...and the viewer has that image to draw          */
    bool pos;           /* a guest position is known for this grab           */
    bool frame;         /* a guest frame is on screen (not the placeholder)  */
    bool ui;            /* the menu or a dialog is open                      */
};

/* Whether the guest cursor is drawn now (before where: the hotspot must also
 * be over the visible picture, nb_view_cursor_rect). */
static inline bool nb_spr_want(const struct nb_spr_in *in)
{
    return in->grabbed && in->focused && in->ptr_in && in->shown &&
           in->image && in->pos && in->frame && !in->ui;
}

/* Grow *acc to cover r (an empty *acc, w or h <= 0, takes r as is). */
static inline void nb_spr_damage_add(struct nb_rect *acc,
                                     const struct nb_rect *r)
{
    int x0, y0, x1, y1;

    if (r->w <= 0 || r->h <= 0) {
        return;
    }
    if (acc->w <= 0 || acc->h <= 0) {
        *acc = *r;
        return;
    }
    x0 = acc->x < r->x ? acc->x : r->x;
    y0 = acc->y < r->y ? acc->y : r->y;
    x1 = acc->x + acc->w > r->x + r->w ? acc->x + acc->w : r->x + r->w;
    y1 = acc->y + acc->h > r->y + r->h ? acc->y + acc->h : r->y + r->h;
    acc->x = x0;
    acc->y = y0;
    acc->w = x1 - x0;
    acc->h = y1 - y0;
}

#endif
