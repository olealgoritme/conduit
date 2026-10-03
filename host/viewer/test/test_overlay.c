/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * test_overlay.c -- the viewer's frame statistics, checked without a
 * compositor: a steady 240 Hz guest with one long frame, presented 5 ms after
 * commit, must read back as such in the snapshot, the graph and the title.
 */
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "../nb_overlay.h"
#include "../nvkvm_broker.h"

static int fails;
#define CHECK(c, ...) do { if (!(c)) { fails++; printf("FAIL %s:%d: ", \
    __FILE__, __LINE__); printf(__VA_ARGS__); printf("\n"); } } while (0)

int main(void)
{
    static struct nb_fstats f;
    struct nb_fsnap s;
    uint32_t graph[100];
    static uint32_t px[400 * 400];
    char title[256];
    uint64_t t = 10ull * 1000000000ull, ms = 1000000ull, last = 0;
    unsigned i, w, h, nz = 0;

    nb_fs_reset(&f);
    nb_fs_roll(&f, t, 0, 0);
    /* 2 s of frames at 240 Hz; flip -> recv 50 us, recv -> commit 10 us,
     * commit -> screen 5 ms; frame 100 takes 20 ms. */
    for (i = 0; i < 480; i++) {
        uint64_t flip = t + (i < 100 ? 0 : 20 * ms - 4166667ull);

        last = flip;
        nb_fs_commit(&f, flip, flip + 50000, flip + 60000);
        nb_fs_presented(&f, flip, flip + 60000, flip + 5060000, true);
        if (i % 60 == 0) {
            nb_fs_roll(&f, flip + 60000, i * 10, 0);
        }
        t += 4166667ull;
    }
    t = last;
    nb_fs_roll(&f, t + 300 * ms, 4800, 2);
    nb_fs_snapshot(&f, t + ms, 1, &s);

    CHECK(s.frames, "frames");
    CHECK(s.fps >= 238 && s.fps <= 241, "fps %.1f", s.fps);
    CHECK(fabs(s.ft_avg - 4.17) < 0.1, "avg %.3f", s.ft_avg);
    CHECK(fabs(s.ft_now - 4.17) < 0.01, "now %.3f", s.ft_now);
    CHECK(s.ft_max < 4.2, "the long frame was more than 1 s ago: max %.3f", s.ft_max);
    CHECK(fabs(s.sr_ms - 0.05) < 0.001, "flip->recv %.4f", s.sr_ms);
    CHECK(fabs(s.rc_ms - 0.01) < 0.001, "recv->commit %.4f", s.rc_ms);
    CHECK(fabs(s.cp_ms - 5.0) < 0.01, "commit->screen %.4f", s.cp_ms);
    CHECK(fabs(s.tot_ms - 5.06) < 0.01, "total %.4f", s.tot_ms);
    CHECK(s.direct == 1, "direct");
    CHECK(s.shown_fps > 200, "shown %.1f", s.shown_fps);

    /* The 2 s graph still sees the 20 ms frame. */
    nb_fs_graph(&f, t + ms, 2000, graph, 100);
    {
        uint32_t mx = 0;

        for (i = 0; i < 100; i++) {
            mx = graph[i] > mx ? graph[i] : mx;
            nz += graph[i] != 0;
        }
        CHECK(mx >= 19000 && mx <= 21000, "graph max %u us", mx);
        CHECK(nz > 90, "graph has %u non-empty bins", nz);
    }

    nb_fs_title(title, sizeof(title), "virtio-nvgpu", 5120, 1440, 239960, &s, true);
    CHECK(strstr(title, "5120x1440@240") && strstr(title, "fps") &&
          strstr(title, "DIRECT"), "title: %s", title);

    nb_ov_size(2, &w, &h);
    CHECK(w <= 400 && h <= 400, "overlay %ux%u", w, h);
    {
        struct nb_ov_info in = { .mode_w = 5120, .mode_h = 1440,
                                 .refresh_mhz = 239960, .scale = 1.0,
                                 .tearing = true, .flip_known = true };

        memset(px, 0, sizeof(px));
        nb_ov_paint(px, w, h, 2, &s, &in, graph, 100);
        CHECK(px[0] != 0, "background painted");
        /* The row just above the graph's baseline: every bin has a bar. */
        CHECK(px[(size_t)(h - 4 * 2 - 2) * w + w / 2] != px[0] &&
              px[(size_t)(h - 4 * 2 - 2) * w + w / 4] != px[0],
              "graph bars painted");
    }

    /* The windowed default: the WHOLE guest picture, centred, never cropped
     * or stretched -- the case the user saw garbage in. */
    {
        static const struct { int bw, bh, ww, wh, dw, dh, ox, oy; } c[] = {
            { 2560, 1440, 1280, 1413, 1280,  720,    0, 346 }, /* tall tile  */
            { 2560, 1440, 5120, 1440, 2560, 1440, 1280,   0 }, /* fs, unswitched */
            { 5120, 1440, 5120, 1440, 5120, 1440,    0,   0 }, /* fs, switched */
            { 2560, 1440, 2556, 1437, 2554, 1437,    1,   0 }, /* 2 px off  */
            { 2560, 1440, 2561, 1440, 2561, 1440,    0,   0 }, /* 1 px: snap */
            { 1920, 1080,  800, 2000,  800,  450,    0, 775 },
        };
        for (i = 0; i < sizeof(c) / sizeof(c[0]); i++) {
            int dw, dh, ox, oy;

            nb_fit(NB_SCALE_ASPECT, c[i].bw, c[i].bh, c[i].ww, c[i].wh,
                   &dw, &dh, &ox, &oy);
            CHECK(dw == c[i].dw && dh == c[i].dh && ox == c[i].ox &&
                  oy == c[i].oy, "fit %dx%d in %dx%d: %dx%d+%d+%d",
                  c[i].bw, c[i].bh, c[i].ww, c[i].wh, dw, dh, ox, oy);
            CHECK(dw <= c[i].ww && dh <= c[i].wh && ox + dw <= c[i].ww &&
                  oy + dh <= c[i].wh, "case %u inside the window", i);
        }
    }

    /* Empty: nothing measured reads as "-", not as zero. */
    nb_fs_reset(&f);
    nb_fs_snapshot(&f, t, -1, &s);
    CHECK(!s.frames && s.ft_avg < 0 && s.cp_ms < 0, "empty snapshot");

    printf(fails ? "overlay stats: %d FAILED\n" : "overlay stats tests passed\n",
           fails);
    return fails != 0;
}
