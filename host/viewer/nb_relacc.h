/* SPDX-License-Identifier: Apache-2.0 */
/*
 * Relative pointer motion in whole pixels, with the fraction carried over.
 *
 * zwp_relative_pointer_v1 reports unaccelerated motion as wl_fixed_t (1/256
 * of a pixel): libinput normalises a mouse to 1000 dpi, so a 1600-dpi mouse
 * moves in steps of 0.625.  Truncating each event to an int dropped every
 * step under a pixel -- slow aiming did not move at all, and fast motion
 * lost up to a pixel per event.  The remainder is kept instead, so the
 * pixels sent always add up to the motion reported (to within one pixel).
 */
#ifndef NB_RELACC_H
#define NB_RELACC_H

#include <stdint.h>

struct nb_relacc {
    int64_t rx, ry;     /* carried-over fraction, in 1/256 pixel */
};

/* Add one event's (fx, fy) in wl_fixed_t; *dx, *dy get the whole pixels
 * to send now (0, 0 when the motion is still under a pixel). */
static inline void nb_relacc_add(struct nb_relacc *a, int32_t fx, int32_t fy,
                                 int *dx, int *dy)
{
    int64_t x = a->rx + fx, y = a->ry + fy;

    *dx = (int)(x / 256);       /* toward zero: the remainder keeps its sign */
    *dy = (int)(y / 256);
    a->rx = x - (int64_t)*dx * 256;
    a->ry = y - (int64_t)*dy * 256;
}

#endif
