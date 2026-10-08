/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * nb_overlay.h -- virtio-nvgpu: frame statistics for the viewer's title and
 * its on-screen overlay.
 *
 * Split from the Wayland backend so the arithmetic (and the pixels) can be
 * checked without a compositor.  The frame path only RECORDS: nb_fs_commit()
 * and nb_fs_presented() are a handful of adds into fixed arrays.  Everything
 * that sorts, divides or draws runs from the backend's clock tick, at most
 * four times a second, never per frame.
 */
#ifndef NB_OVERLAY_H
#define NB_OVERLAY_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#define NB_FS_SAMPLES   2048        /* flip intervals kept (>2 s at 1000 fps) */
#define NB_FS_BUCKET_MS 250         /* rate counters roll this often          */
#define NB_FS_BUCKETS   4           /* ...and are summed over this many: 1 s  */

/* One quarter-second of counters. */
struct nb_fs_bucket {
    unsigned commits, presented, zero_copy, superseded;
    uint64_t sr_sum; unsigned sr_n;     /* guest flip -> broker recv, ns  */
    uint64_t rc_sum; unsigned rc_n;     /* recv -> wl_surface_commit, ns  */
    uint64_t cp_sum; unsigned cp_n;     /* commit -> on screen, ns        */
    uint64_t tot_sum; unsigned tot_n;   /* flip (or recv) -> on screen, ns*/
    uint64_t input, rejected;           /* deltas of the sink's counters  */
};

struct nb_fstats {
    uint64_t t_ns[NB_FS_SAMPLES];       /* when each flip happened        */
    uint32_t dt_us[NB_FS_SAMPLES];      /* interval to the flip before it */
    unsigned head, n;
    uint64_t last_flip_ns;
    struct nb_fs_bucket cur, hist[NB_FS_BUCKETS];
    unsigned hist_at, hist_n;
    uint64_t bucket_t0_ns;
    uint64_t input_seen, rejected_seen; /* sink totals at the last roll   */
    bool     flip_known;                /* sr/tot measured from the flip  */
};

/* What the title and the overlay print.  Negative = not measured. */
struct nb_fsnap {
    double fps, shown_fps;
    double ft_now, ft_avg, ft_p99, ft_max;          /* ms over the last 1 s */
    double sr_ms, rc_ms, cp_ms, tot_ms;
    double superseded_ps, rejected_ps, input_ps;
    int    direct;              /* -1 unknown, 0 composited, 1 zero-copy    */
    bool   frames;              /* any frame in the last second             */
};

void nb_fs_reset(struct nb_fstats *f);
/* A frame was committed.  flip_ns is the guest's flip time when the client
 * stamps it (seq usec), else 0; recv_ns when its ATTACH was read. */
void nb_fs_commit(struct nb_fstats *f, uint64_t flip_ns, uint64_t recv_ns,
                  uint64_t commit_ns);
/* wp_presentation said it reached the screen at present_ns. */
void nb_fs_presented(struct nb_fstats *f, uint64_t start_ns, uint64_t commit_ns,
                     uint64_t present_ns, bool zero_copy);
void nb_fs_superseded(struct nb_fstats *f);
/* Called from the clock tick.  Closes the current quarter second when due;
 * input/rejected are the sink's running totals. */
bool nb_fs_roll(struct nb_fstats *f, uint64_t now_ns, uint64_t input_total,
                uint64_t rejected_total);
void nb_fs_snapshot(const struct nb_fstats *f, uint64_t now_ns, int last_direct,
                    struct nb_fsnap *out);
/* Worst flip interval (us) in each of `cols` equal time bins covering the last
 * `span_ms`, oldest first; 0 where no flip landed. */
void nb_fs_graph(const struct nb_fstats *f, uint64_t now_ns, unsigned span_ms,
                 uint32_t *out_us, unsigned cols);

/*
 * How the guest's bw x bh picture goes into a ww x wh window, per --scale
 * (NB_SCALE_*): its size there and its offset.  ASPECT (the default) is the
 * whole picture, as large as fits, centred -- never cropped, never stretched.
 */
void nb_fit(int mode, int bw, int bh, int ww, int wh, int *dw, int *dh,
            int *ox, int *oy);

/*
 * Mode-hint flap guard.  The sizes the guest was last asked for (a change of
 * size only: a hint that repeats the previous size is not a re-mode), and
 * whether asking for w x h now would be the fourth step of an A,B,A,B flip
 * inside window_ms -- the shape of a window bouncing between two sizes, where
 * every step is a mode set in the guest.  Pure, so test/test_overlay.c checks
 * it without a compositor.
 */
#define NB_HINT_HIST 3
struct nb_hint_hist {
    unsigned n;                 /* valid entries, newest first              */
    unsigned w[NB_HINT_HIST], h[NB_HINT_HIST];
    uint64_t ms[NB_HINT_HIST];
};
void nb_hint_note(struct nb_hint_hist *hh, unsigned w, unsigned h,
                  uint64_t now_ms);
bool nb_hint_flaps(const struct nb_hint_hist *hh, unsigned w, unsigned h,
                   uint64_t now_ms, unsigned window_ms);

/* The one-line form, for xdg_toplevel.set_title. */
void nb_fs_title(char *buf, size_t n, const char *base, unsigned mode_w,
                 unsigned mode_h, unsigned refresh_mhz,
                 const struct nb_fsnap *s, bool flip_known);

/* The overlay, painted into premultiplied ARGB8888. */
struct nb_ov_info {
    unsigned mode_w, mode_h, refresh_mhz;
    double   scale;             /* guest pixel -> output pixel              */
    bool     tearing;           /* ASYNC requested                          */
    bool     direct_mode;
    bool     guest_resize;      /* --resize=guest                           */
    bool     flip_known;
    bool     host_cursor;       /* the guest cursor is the host pointer     */
};
void nb_ov_size(unsigned scale, unsigned *w, unsigned *h);
void nb_ov_paint(uint32_t *px, unsigned w, unsigned h, unsigned scale,
                 const struct nb_fsnap *s, const struct nb_ov_info *in,
                 const uint32_t *graph_us, unsigned graph_cols);

#endif /* NB_OVERLAY_H */
