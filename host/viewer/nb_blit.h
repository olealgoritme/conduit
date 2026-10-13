/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * nb_blit.h -- how a guest frame reaches the compositor: attached as it is,
 * or copied by the viewer's GPU pass.
 *
 * The compositor takes only the (format, modifier) pairs it advertises, and
 * not always those (a pair can be advertised and then refused on import).
 * A guest frame in any other layout is still displayable when the viewer's
 * GPU can import it: the GPU pass (nb_gpuscale.h) draws it into a buffer the
 * compositor does take.  That costs one GPU pass per frame, so a direct
 * attach is always preferred; the copy is the fallback that keeps the window
 * from going black, in every window mode.
 *
 * Pure logic: test/test_blit.c.
 */
#ifndef NB_BLIT_H
#define NB_BLIT_H

#include <stdbool.h>

enum nb_att {
    NB_ATT_DIRECT = 0,  /* a wl_buffer of the guest's own dma-buf          */
    NB_ATT_COPY,        /* no wl_buffer; the GPU pass draws every frame    */
    NB_ATT_REJECT,      /* neither: the frame cannot be shown              */
};

/*
 * advertised: the compositor lists the pair (or its opaque twin).
 * proven:     1 the compositor imported it, 0 not tried yet, -1 refused.
 * copy_ok:    the GPU pass is available and the viewer's GPU imports it.
 */
static inline enum nb_att nb_att_decide(bool advertised, int proven,
                                        bool copy_ok)
{
    if (advertised && proven >= 0) {
        return NB_ATT_DIRECT;
    }
    return copy_ok ? NB_ATT_COPY : NB_ATT_REJECT;
}

/*
 * Whether the GPU copy may be offered at all: a dma-buf path that is not
 * deliberately restricted (--present-mode=linear makes the VMM detile;
 * shm advertises no dma-buf), a scaler that is enabled and has not failed,
 * and a viewport to place its output with.
 */
static inline bool nb_copy_allowed(bool tier_native_or_auto, bool gs_enabled,
                                   bool gs_dead, bool have_viewport,
                                   bool have_dmabuf)
{
    return tier_native_or_auto && gs_enabled && !gs_dead && have_viewport &&
           have_dmabuf;
}

/*
 * Whether this frame goes through the GPU pass.  A copy-only frame always
 * does (windowed or fullscreen, any scale or area: there is nothing else to
 * attach).  A directly attachable one only for the fullscreen scaler:
 * fullscreen, immediate or latch presentation, and not already 1:1.
 */
static inline bool nb_frame_uses_gpu(bool copy_only, bool gs_usable,
                                     bool fullscreen, bool tear_or_latch,
                                     bool exact)
{
    if (!gs_usable) {
        return false;
    }
    if (copy_only) {
        return true;
    }
    return fullscreen && tear_or_latch && !exact;
}

#endif /* NB_BLIT_H */
