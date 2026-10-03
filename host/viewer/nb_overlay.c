/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * nb_overlay.c -- virtio-nvgpu: frame statistics, the title line and the
 * overlay's pixels.  See nb_overlay.h for the cost rules.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "nb_overlay.h"
#include "nvkvm_broker.h"

#define NS_PER_MS 1000000ull

void nb_fs_reset(struct nb_fstats *f)
{
    memset(f, 0, sizeof(*f));
}

void nb_fs_commit(struct nb_fstats *f, uint64_t flip_ns, uint64_t recv_ns,
                  uint64_t commit_ns)
{
    uint64_t t = flip_ns ? flip_ns : recv_ns;

    f->cur.commits++;
    f->flip_known = flip_ns != 0;
    if (flip_ns && recv_ns >= flip_ns) {
        f->cur.sr_sum += recv_ns - flip_ns;
        f->cur.sr_n++;
    }
    if (recv_ns && commit_ns >= recv_ns) {
        f->cur.rc_sum += commit_ns - recv_ns;
        f->cur.rc_n++;
    }
    if (!t) {
        return;
    }
    if (f->last_flip_ns && t > f->last_flip_ns) {
        uint64_t d = (t - f->last_flip_ns) / 1000u;

        f->t_ns[f->head] = t;
        f->dt_us[f->head] = d > UINT32_MAX ? UINT32_MAX : (uint32_t)d;
        f->head = (f->head + 1) % NB_FS_SAMPLES;
        if (f->n < NB_FS_SAMPLES) {
            f->n++;
        }
    }
    if (t > f->last_flip_ns) {
        f->last_flip_ns = t;
    }
}

void nb_fs_presented(struct nb_fstats *f, uint64_t start_ns, uint64_t commit_ns,
                     uint64_t present_ns, bool zero_copy)
{
    f->cur.presented++;
    f->cur.zero_copy += zero_copy;
    if (commit_ns && present_ns >= commit_ns) {
        f->cur.cp_sum += present_ns - commit_ns;
        f->cur.cp_n++;
    }
    if (start_ns && present_ns >= start_ns &&
        present_ns - start_ns < 1000 * NS_PER_MS) {
        f->cur.tot_sum += present_ns - start_ns;
        f->cur.tot_n++;
    }
}

void nb_fs_superseded(struct nb_fstats *f)
{
    f->cur.superseded++;
}

bool nb_fs_roll(struct nb_fstats *f, uint64_t now_ns, uint64_t input_total,
                uint64_t rejected_total)
{
    if (!f->bucket_t0_ns) {
        f->bucket_t0_ns = now_ns;
        f->input_seen = input_total;
        f->rejected_seen = rejected_total;
        return false;
    }
    if (now_ns - f->bucket_t0_ns < NB_FS_BUCKET_MS * NS_PER_MS) {
        return false;
    }
    /* A sink counter that went backwards was reset (a new client). */
    f->cur.input = input_total >= f->input_seen ? input_total - f->input_seen
                                                : input_total;
    f->cur.rejected = rejected_total >= f->rejected_seen
                          ? rejected_total - f->rejected_seen : rejected_total;
    f->input_seen = input_total;
    f->rejected_seen = rejected_total;
    /* Rates divide by NB_FS_BUCKET_MS per bucket: the backend ticks this
     * every quarter second while a client is attached, so a bucket is that
     * long give or take a poll wakeup. */
    f->hist[f->hist_at] = f->cur;
    f->hist_at = (f->hist_at + 1) % NB_FS_BUCKETS;
    if (f->hist_n < NB_FS_BUCKETS) {
        f->hist_n++;
    }
    memset(&f->cur, 0, sizeof(f->cur));
    f->bucket_t0_ns = now_ns;
    return true;
}

static int cmp_u32(const void *a, const void *b)
{
    uint32_t x = *(const uint32_t *)a, y = *(const uint32_t *)b;

    return x < y ? -1 : x > y;
}

void nb_fs_snapshot(const struct nb_fstats *f, uint64_t now_ns, int last_direct,
                    struct nb_fsnap *o)
{
    static uint32_t tmp[NB_FS_SAMPLES];  /* single-threaded broker */
    struct nb_fs_bucket sum = {0};
    unsigned i, k = 0, idx;
    double secs = f->hist_n * (NB_FS_BUCKET_MS / 1000.0);
    uint64_t total = 0, newest_t = 0;
    uint32_t newest = 0, mx = 0;

    memset(o, 0, sizeof(*o));
    o->direct = last_direct;
    for (i = 0; i < f->hist_n; i++) {
        const struct nb_fs_bucket *b = &f->hist[i];

        sum.commits += b->commits;
        sum.presented += b->presented;
        sum.zero_copy += b->zero_copy;
        sum.superseded += b->superseded;
        sum.sr_sum += b->sr_sum; sum.sr_n += b->sr_n;
        sum.rc_sum += b->rc_sum; sum.rc_n += b->rc_n;
        sum.cp_sum += b->cp_sum; sum.cp_n += b->cp_n;
        sum.tot_sum += b->tot_sum; sum.tot_n += b->tot_n;
        sum.input += b->input;
        sum.rejected += b->rejected;
    }
    if (secs > 0) {
        o->shown_fps = sum.presented / secs;
        o->superseded_ps = sum.superseded / secs;
        o->rejected_ps = (double)sum.rejected / secs;
        o->input_ps = (double)sum.input / secs;
    }
    o->sr_ms = sum.sr_n ? sum.sr_sum / (double)sum.sr_n / 1e6 : -1;
    o->rc_ms = sum.rc_n ? sum.rc_sum / (double)sum.rc_n / 1e6 : -1;
    o->cp_ms = sum.cp_n ? sum.cp_sum / (double)sum.cp_n / 1e6 : -1;
    o->tot_ms = sum.tot_n ? sum.tot_sum / (double)sum.tot_n / 1e6 : -1;

    /* Flip intervals over the last second, newest first. */
    for (i = 0; i < f->n; i++) {
        idx = (f->head + NB_FS_SAMPLES - 1 - i) % NB_FS_SAMPLES;
        if (f->t_ns[idx] + 1000 * NS_PER_MS < now_ns) {
            break;
        }
        if (f->t_ns[idx] > newest_t) {
            newest_t = f->t_ns[idx];
            newest = f->dt_us[idx];
        }
        tmp[k++] = f->dt_us[idx];
        total += f->dt_us[idx];
        if (f->dt_us[idx] > mx) {
            mx = f->dt_us[idx];
        }
    }
    o->fps = k;
    o->frames = k > 0;
    if (!k) {
        o->ft_now = o->ft_avg = o->ft_p99 = o->ft_max = -1;
        return;
    }
    qsort(tmp, k, sizeof(tmp[0]), cmp_u32);
    o->ft_now = newest / 1000.0;
    o->ft_avg = (double)total / k / 1000.0;
    o->ft_p99 = tmp[(k * 99) / 100 < k ? (k * 99) / 100 : k - 1] / 1000.0;
    o->ft_max = mx / 1000.0;
}

void nb_fs_graph(const struct nb_fstats *f, uint64_t now_ns, unsigned span_ms,
                 uint32_t *out, unsigned cols)
{
    uint64_t span = (uint64_t)span_ms * NS_PER_MS, start;
    unsigned i, idx;

    memset(out, 0, cols * sizeof(*out));
    if (!cols || now_ns < span) {
        return;
    }
    start = now_ns - span;
    for (i = 0; i < f->n; i++) {
        uint64_t t;
        unsigned c;

        idx = (f->head + NB_FS_SAMPLES - 1 - i) % NB_FS_SAMPLES;
        t = f->t_ns[idx];
        if (t < start) {
            break;
        }
        c = (unsigned)((t - start) * cols / (span + 1));
        if (c < cols && f->dt_us[idx] > out[c]) {
            out[c] = f->dt_us[idx];
        }
    }
}

void nb_fit(int mode, int bw, int bh, int ww, int wh, int *dw, int *dh,
            int *ox, int *oy)
{
    int w, h;

    if (bw <= 0 || bh <= 0) {
        bw = ww > 0 ? ww : 1;
        bh = wh > 0 ? wh : 1;
    }
    if (mode == NB_SCALE_NONE || ww <= 0 || wh <= 0) {
        w = bw;                 /* 1:1, whatever the window is */
        h = bh;
    } else if (mode == NB_SCALE_STRETCH) {
        w = ww;
        h = wh;
    } else {
        /*
         * ASPECT.  As large as fits, aspect kept, remainder blank.  Both
         * branches use the same rounding, and a remainder of one pixel snaps
         * to the window: a window whose aspect MATCHES the picture's must come
         * out exactly equal to it -- no 1 px bar -- or the surface does not
         * cover the output and direct scanout is quietly lost.
         */
        long by_h = (long)wh * bw / bh;     /* width if we fit by height */

        if (by_h <= (long)ww) {
            w = (int)by_h;
            h = wh;
        } else {
            w = ww;
            h = (int)((long)ww * bh / bw);
        }
        if (w > ww - 2 && w < ww + 2) {
            w = ww;
        }
        if (h > wh - 2 && h < wh + 2) {
            h = wh;
        }
    }
    if (w <= 0 || h <= 0) {
        w = bw;
        h = bh;
    }
    *dw = w;
    *dh = h;
    *ox = ww > w ? (ww - w) / 2 : 0;
    *oy = wh > h ? (wh - h) / 2 : 0;
}

/* ms with a precision that suits the magnitude; "-" when not measured. */
static void fmt_ms(char *b, size_t n, double ms)
{
    if (ms < 0) {
        snprintf(b, n, "-");
    } else if (ms < 1.0) {
        snprintf(b, n, "%.2f", ms);
    } else if (ms < 100.0) {
        snprintf(b, n, "%.1f", ms);
    } else {
        snprintf(b, n, "%.0f", ms);
    }
}

/* The title wants units a person reads at a glance: µs below a millisecond. */
static void fmt_lat(char *b, size_t n, double ms)
{
    if (ms < 0) {
        snprintf(b, n, "-");
    } else if (ms < 1.0) {
        snprintf(b, n, "%.0f \xc2\xb5s", ms * 1000.0);
    } else {
        snprintf(b, n, "%.1f ms", ms);
    }
}

void nb_fs_title(char *buf, size_t n, const char *base, unsigned mode_w,
                 unsigned mode_h, unsigned refresh_mhz,
                 const struct nb_fsnap *s, bool flip_known)
{
    char hz[16] = "", fc[24], cs[24];
    double fc_ms = s->rc_ms;

    if (refresh_mhz) {
        snprintf(hz, sizeof(hz), "@%u", (refresh_mhz + 500) / 1000);
    }
    if (!s->frames || !mode_w) {
        snprintf(buf, n, "%s | no frames", base);
        return;
    }
    if (flip_known && s->sr_ms >= 0 && s->rc_ms >= 0) {
        fc_ms = s->sr_ms + s->rc_ms;
    }
    fmt_lat(fc, sizeof(fc), fc_ms);
    fmt_lat(cs, sizeof(cs), s->cp_ms);
    snprintf(buf, n,
             "%s %ux%u%s | %.0f fps | frame %.1f ms (p99 %.1f) | "
             "%s\xe2\x86\x92" "commit %s | commit\xe2\x86\x92screen %s | %s",
             base, mode_w, mode_h, hz, s->fps, s->ft_avg, s->ft_p99,
             flip_known ? "flip" : "recv", fc, cs,
             s->direct > 0 ? "DIRECT" : s->direct == 0 ? "COMPOSITED" : "?");
}

/* ── overlay pixels ─────────────────────────────────────────────────────── */

#define OV_COLS   28            /* characters per line                      */
#define OV_LINES  12
#define OV_GRAPH  24            /* graph height, in font pixels             */

/* Premultiplied ARGB8888. */
#define OV_BG     0xd80c0f12u
#define OV_FG     0xffe6edf3u
#define OV_DIM    0xff7d8590u
#define OV_GREEN  0xff3fb950u
#define OV_YELLOW 0xffd29922u
#define OV_RED    0xfff85149u
#define OV_CYAN   0xff39c5cfu

void nb_ov_size(unsigned sc, unsigned *w, unsigned *h)
{
    if (!sc) {
        sc = 1;
    }
    *w = (OV_COLS * 6 + 8) * sc;
    *h = (8 + OV_LINES * 10 + 2 + OV_GRAPH) * sc;
}

static void ov_line(uint32_t *px, unsigned w, unsigned h, unsigned sc,
                    unsigned line, const char *txt, uint32_t c)
{
    nb_placeholder_text(px, w, h, w, 4 * sc, (4 + line * 10) * sc, txt, sc, c);
}

void nb_ov_paint(uint32_t *px, unsigned w, unsigned h, unsigned sc,
                 const struct nb_fsnap *s, const struct nb_ov_info *in,
                 const uint32_t *graph, unsigned cols)
{
    char l[64], a[16], b[16], c[16], d[16];
    double target = in->refresh_mhz ? 1e6 / in->refresh_mhz : 1000.0 / 60.0;
    unsigned gx0 = 4 * sc, gy0 = (4 + OV_LINES * 10 + 2) * sc;
    unsigned gw = w > 8 * sc ? w - 8 * sc : 0, gh = OV_GRAPH * sc;
    unsigned i, x, y;

    nb_placeholder_fill(px, w, h, w, OV_BG);
    ov_line(px, w, h, sc, 0, "CONDUIT", OV_CYAN);

    if (in->mode_w) {
        snprintf(l, sizeof(l), "%uX%u@%u SCALE %.2f", in->mode_w, in->mode_h,
                 (in->refresh_mhz + 500) / 1000, in->scale);
    } else {
        snprintf(l, sizeof(l), "NO GUEST FRAME YET");
    }
    ov_line(px, w, h, sc, 1, l, OV_FG);
    snprintf(l, sizeof(l), "%s %s %s",
             s->direct > 0 ? "DIRECT" : s->direct == 0 ? "COMPOSITED" : "-",
             in->tearing ? "ASYNC" : "VSYNC",
             in->guest_resize ? "FOLLOW" : "FIT");
    ov_line(px, w, h, sc, 2, l, s->direct > 0 ? OV_GREEN : OV_YELLOW);
    snprintf(l, sizeof(l), "FPS %.0f SHOWN %.0f", s->fps, s->shown_fps);
    ov_line(px, w, h, sc, 3, l, OV_FG);
    fmt_ms(a, sizeof(a), s->ft_now);
    fmt_ms(b, sizeof(b), s->ft_avg);
    snprintf(l, sizeof(l), "FRAME %s AVG %s MS", a, b);
    ov_line(px, w, h, sc, 4, l, OV_FG);
    fmt_ms(a, sizeof(a), s->ft_p99);
    fmt_ms(b, sizeof(b), s->ft_max);
    snprintf(l, sizeof(l), "  P99 %s MAX %s MS", a, b);
    ov_line(px, w, h, sc, 5, l, OV_FG);
    fmt_ms(a, sizeof(a), in->flip_known ? s->sr_ms : -1);
    fmt_ms(b, sizeof(b), s->rc_ms);
    fmt_ms(c, sizeof(c), s->cp_ms);
    fmt_ms(d, sizeof(d), s->tot_ms);
    snprintf(l, sizeof(l), "FLIP>RECV     %s MS", a);
    ov_line(px, w, h, sc, 6, l, in->flip_known ? OV_FG : OV_DIM);
    snprintf(l, sizeof(l), "RECV>COMMIT   %s MS", b);
    ov_line(px, w, h, sc, 7, l, OV_FG);
    snprintf(l, sizeof(l), "COMMIT>SCREEN %s MS", c);
    ov_line(px, w, h, sc, 8, l, OV_FG);
    snprintf(l, sizeof(l), "%s %s MS", in->flip_known ? "FLIP>SCREEN  "
                                                      : "RECV>SCREEN  ", d);
    ov_line(px, w, h, sc, 9, l, OV_CYAN);
    snprintf(l, sizeof(l), "DROP %.0f SUPERSEDED %.0f /S", s->rejected_ps,
             s->superseded_ps);
    ov_line(px, w, h, sc, 10, l,
            s->rejected_ps > 0 || s->superseded_ps > 0 ? OV_YELLOW : OV_FG);
    snprintf(l, sizeof(l), "INPUT %.0f/S CURSOR %s", s->input_ps,
             in->host_cursor ? "HOST" : "GUEST");
    ov_line(px, w, h, sc, 11, l, OV_FG);

    /* The graph: the worst flip interval per bin over the last 2 s, scaled
     * so the refresh interval sits at 40% height. */
    if (!gw || !cols || gy0 + gh > h) {
        return;
    }
    {
        double top = target * 2.5;
        unsigned ty = gy0 + gh - (unsigned)(gh * (1.0 / 2.5));

        for (x = 0; x < gw; x++) {
            px[(size_t)ty * w + gx0 + x] = OV_DIM;
            px[(size_t)(gy0 + gh - 1) * w + gx0 + x] = OV_DIM;
        }
        for (i = 0; i < gw; i++) {
            uint32_t v = graph[(size_t)i * cols / gw];
            double ms = v / 1000.0;
            unsigned bh;
            uint32_t col;

            if (!v) {
                continue;
            }
            bh = ms >= top ? gh : (unsigned)(gh * ms / top);
            if (bh < 1) {
                bh = 1;
            }
            col = ms <= target * 1.5 ? OV_GREEN
                : ms <= target * 2.5 ? OV_YELLOW : OV_RED;
            for (y = 0; y < bh; y++) {
                px[(size_t)(gy0 + gh - 1 - y) * w + gx0 + i] = col;
            }
        }
    }
}
