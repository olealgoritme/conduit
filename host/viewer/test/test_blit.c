/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * test_blit.c -- direct attach or GPU copy (nb_blit.h).
 */
#include <stdio.h>

#include "nb_blit.h"

static int checks, fails;
#define CHECK(c, ...) do { checks++; if (!(c)) { fails++; \
        printf("FAIL %s:%d: ", __FILE__, __LINE__); printf(__VA_ARGS__); \
        printf("\n"); } } while (0)

int main(void)
{
    /* Advertised and not refused: direct, whatever the copy could do. */
    CHECK(nb_att_decide(true, 0, true) == NB_ATT_DIRECT, "advertised");
    CHECK(nb_att_decide(true, 1, false) == NB_ATT_DIRECT, "proven");
    /* Not advertised (the live case: AR24 NVIDIA block-linear on a display
     * that lists 20 other pairs): the GPU copy, not a dropped frame. */
    CHECK(nb_att_decide(false, 0, true) == NB_ATT_COPY, "not advertised");
    /* Advertised but refused on import: also the copy. */
    CHECK(nb_att_decide(true, -1, true) == NB_ATT_COPY, "refused");
    /* No copy possible: rejected, as before. */
    CHECK(nb_att_decide(false, 0, false) == NB_ATT_REJECT, "no copy");
    CHECK(nb_att_decide(true, -1, false) == NB_ATT_REJECT, "refused, no copy");

    /* When the copy may be offered. */
    CHECK(nb_copy_allowed(true, true, false, true, true), "allowed");
    CHECK(!nb_copy_allowed(false, true, false, true, true), "linear/shm tier");
    CHECK(!nb_copy_allowed(true, false, false, true, true), "--gpu-scale=off");
    CHECK(!nb_copy_allowed(true, true, true, true, true), "scaler failed");
    CHECK(!nb_copy_allowed(true, true, false, false, true), "no viewporter");
    CHECK(!nb_copy_allowed(true, true, false, true, false), "no dmabuf");

    /* A copy-only frame uses the pass in every window mode. */
    for (int fs = 0; fs < 2; fs++) {
        for (int tl = 0; tl < 2; tl++) {
            for (int ex = 0; ex < 2; ex++) {
                CHECK(nb_frame_uses_gpu(true, true, fs, tl, ex),
                      "copy-only fs=%d tl=%d exact=%d", fs, tl, ex);
                CHECK(!nb_frame_uses_gpu(true, false, fs, tl, ex),
                      "pass unusable fs=%d tl=%d exact=%d", fs, tl, ex);
            }
        }
    }
    /* A direct frame: only the fullscreen scaler. */
    CHECK(!nb_frame_uses_gpu(false, true, false, true, false), "windowed");
    CHECK(!nb_frame_uses_gpu(false, true, true, false, false), "vsync");
    CHECK(!nb_frame_uses_gpu(false, true, true, true, true), "1:1");
    CHECK(nb_frame_uses_gpu(false, true, true, true, false), "scaler");

    printf("test_blit: %d checks, %d failed\n", checks, fails);
    return fails ? 1 : 0;
}
