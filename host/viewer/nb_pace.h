/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * nb_pace.h -- when the viewer may commit a guest frame to the compositor.
 *
 * A guest can flip far faster than any compositor presents: an uncapped
 * exclusive-fullscreen app with immediate flips sends thousands of frames a
 * second.  Committing each one writes ~200 bytes of requests (attach, damage,
 * frame callback, presentation feedback, commit) to the compositor socket,
 * and libwayland treats a full socket buffer (EAGAIN while marshalling) as a
 * FATAL error: the display connection dies, the viewer detaches.
 *
 * So a frame is committed only when the compositor can use it:
 *
 *   - never while the socket is backed up (the last flush hit EAGAIN);
 *   - after a frame callback was armed, at most NB_PACE_AHEAD commits until
 *     it fires (the compositor shows the newest of them anyway);
 *   - if the callback never comes (window hidden, compositor wedged), one
 *     commit per NB_PACE_STUCK_NS so the surface does not freeze.
 *
 * A frame that may not be committed now stays staged; a newer ATTACH
 * replaces it (and releases it to the guest at once), and the staged one is
 * committed as soon as the gate opens.  Latest frame wins, nothing queues.
 *
 * Pure logic, no Wayland: test/test_pace.c drives it.
 */
#ifndef NB_PACE_H
#define NB_PACE_H

#include <stdbool.h>
#include <stdint.h>

#define NB_PACE_AHEAD    2                      /* commits per frame callback */
#define NB_PACE_STUCK_NS (50ull * 1000 * 1000)  /* callback overdue: 20 Hz    */

struct nb_pace {
    bool     armed;             /* a frame callback is outstanding         */
    unsigned ahead;             /* commits since it was armed              */
    uint64_t last_commit_ns;
    bool     blocked;           /* the compositor socket is full           */
    /* counters, for --stats */
    uint64_t deferred;          /* commit() calls that were held back      */
};

/* May a frame be committed now? */
static inline bool nb_pace_may_commit(const struct nb_pace *p, uint64_t now)
{
    if (p->blocked) {
        return false;
    }
    if (!p->armed || p->ahead < NB_PACE_AHEAD) {
        return true;
    }
    return now - p->last_commit_ns >= NB_PACE_STUCK_NS;
}

/* A frame was committed; `armed_now`: a frame callback was requested with it. */
static inline void nb_pace_committed(struct nb_pace *p, uint64_t now,
                                     bool armed_now)
{
    if (armed_now) {
        p->armed = true;
        p->ahead = 0;
    }
    p->ahead++;
    p->last_commit_ns = now;
}

/* The frame callback fired (or was abandoned with its surface). */
static inline void nb_pace_frame_done(struct nb_pace *p)
{
    p->armed = false;
    p->ahead = 0;
}

/* Result of the last non-blocking flush. */
static inline void nb_pace_flushed(struct nb_pace *p, bool eagain)
{
    p->blocked = eagain;
}

/* When to look again for a staged frame held back by an overdue callback:
 * nanoseconds from now, 0 = no deadline (an event will open the gate). */
static inline uint64_t nb_pace_due_ns(const struct nb_pace *p, uint64_t now)
{
    uint64_t at;

    if (p->blocked || !p->armed || p->ahead < NB_PACE_AHEAD) {
        return 0;
    }
    at = p->last_commit_ns + NB_PACE_STUCK_NS;
    return at > now ? at - now : 1;
}

/*
 * THE CURSOR NUDGE.  While a compositor tears our window it renders only when
 * we commit: Hyprland skips its vblank repaint for an actively tearing
 * output, so a SOFTWARE cursor (WLR_NO_HARDWARE_CURSORS, common on NVIDIA)
 * is redrawn only with our frames.  With the guest on a hardware cursor an
 * idle desktop sends 5-50 frames a second and the pointer stutters.  So on
 * pointer motion (and a cursor image change) over a fullscreen, immediately
 * presented picture, the current buffer is committed again with a token
 * damage, at most every `min_ns` and only through the same gate as frames.
 */
static inline bool nb_nudge_wanted(bool immediate, bool fullscreen,
                                   bool cursor_visible, bool have_frame)
{
    return immediate && fullscreen && cursor_visible && have_frame;
}

/* Nudge now?  Else the caller keeps it pending and asks nb_nudge_due_ns(). */
static inline bool nb_nudge_may(const struct nb_pace *p, uint64_t now,
                                uint64_t min_ns)
{
    return nb_pace_may_commit(p, now) && now - p->last_commit_ns >= min_ns;
}

/* ns until a pending nudge may go (>= 1), or 0 when an event must open the
 * gate first (socket full, callback outstanding). */
static inline uint64_t nb_nudge_due_ns(const struct nb_pace *p, uint64_t now,
                                       uint64_t min_ns)
{
    uint64_t at;

    if (!nb_pace_may_commit(p, now)) {
        return nb_pace_due_ns(p, now);
    }
    at = p->last_commit_ns + min_ns;
    return at > now ? at - now : 1;
}

#endif /* NB_PACE_H */
