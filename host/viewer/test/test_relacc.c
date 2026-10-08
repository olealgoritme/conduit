/* SPDX-License-Identifier: Apache-2.0 */
/* nb_relacc.h: sub-pixel relative motion is carried, never dropped. */
#include <assert.h>
#include <stdio.h>
#include "nb_relacc.h"

int main(void)
{
    struct nb_relacc a = { 0, 0 };
    int dx, dy, sx = 0, sy = 0, i;

    /* 0.625 px steps (1600 dpi at libinput's 1000 dpi): 8 make 5 px. */
    for (i = 0; i < 8; i++) {
        nb_relacc_add(&a, 160, -160, &dx, &dy);
        sx += dx;
        sy += dy;
    }
    assert(sx == 5 && sy == -5);
    assert(a.rx == 0 && a.ry == 0);

    /* Under a pixel: nothing yet, then it arrives. */
    nb_relacc_add(&a, 100, 0, &dx, &dy);
    assert(dx == 0 && dy == 0);
    nb_relacc_add(&a, 200, 0, &dx, &dy);
    assert(dx == 1 && a.rx == 44);

    /* Back and forth cancels exactly. */
    nb_relacc_add(&a, -44 - 3 * 256, 0, &dx, &dy);
    assert(dx == -3 && a.rx == 0);

    /* Whole pixels pass through unchanged. */
    nb_relacc_add(&a, 7 * 256, -2 * 256, &dx, &dy);
    assert(dx == 7 && dy == -2);
    puts("test-relacc: ok");
    return 0;
}
