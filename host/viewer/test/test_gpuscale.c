/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * test_gpuscale.c -- the fullscreen scaler on a real GPU, with no display.
 *
 * A 160x90 LINEAR source (quadrants: red top-left, green bottom-left, blue
 * on the right) is drawn letterboxed into a 640x180 target, read back
 * through GBM and checked pixel by pixel: the bars are black, the picture
 * lands where nb_view placed it, and nothing is flipped.  Exits 77 (skip)
 * when there is no render node, EGL or mappable GBM.
 */
#include <fcntl.h>
#include <gbm.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <time.h>

#include "nb_gpuscale.h"
#include "nb_view.h"

static int checks, fails;
#define CHECK(c, ...) do { checks++; if (!(c)) { fails++; \
        printf("FAIL %s:%d: ", __FILE__, __LINE__); printf(__VA_ARGS__); \
        printf("\n"); } } while (0)

#define SW 160
#define SH 90
#define TW 640
#define TH 180

static uint32_t px(struct nb_gs *g, int t, int x, int y)
{
    uint32_t c = 0xdeadbe;

    nb_gs_read(g, t, x, y, &c);
    return c;
}

/* 500 draws at bw x bh; with `mem`, each read back into it (in the cost). */
static void bench(struct nb_gs *g, const char *what, unsigned bw, unsigned bh,
                  uint32_t *mem)
{
    unsigned sum = 0, mx = 0, us;
    static unsigned v[500];

    for (int i = 0; i < 500; i++) {
        struct nb_gs_draw d = {
            .src_key = 3, .target = i % NB_GS_TARGETS, .dx = 0, .dy = 0,
            .dw = (int)bw, .dh = (int)bh, .sx = 0, .sy = 0, .sw = SW, .sh = SH,
        };
        struct timespec a, b;

        clock_gettime(CLOCK_MONOTONIC, &a);
        nb_gs_draw(g, &d, 1000000, &us);
        if (mem) {
            nb_gs_readback(g, d.target, mem);
        }
        clock_gettime(CLOCK_MONOTONIC, &b);
        us = (unsigned)((b.tv_sec - a.tv_sec) * 1000000 +
                        (b.tv_nsec - a.tv_nsec) / 1000);
        v[i] = us;
        sum += us;
        mx = us > mx ? us : mx;
    }
    for (int i = 1; i < 500; i++) {         /* p99 by insertion */
        unsigned k = v[i];
        int j = i - 1;

        while (j >= 0 && v[j] > k) {
            v[j + 1] = v[j];
            j--;
        }
        v[j + 1] = k;
    }
    printf("test_gpuscale: %ux%u %s: avg %u us, p50 %u us, p99 %u us, max %u "
           "us\n", bw, bh, what, sum / 500, v[250], v[495], mx);
}

/* A LINEAR XRGB8888 source: red left of `edge`, blue from it on. */
static struct gbm_bo *edge_src(struct gbm_device *dev, int edge)
{
    struct gbm_bo *bo = gbm_bo_create(dev, SW, SH, GBM_FORMAT_XRGB8888,
                                      GBM_BO_USE_LINEAR);
    void *map = NULL;
    uint32_t stride;
    uint8_t *p;

    if (!bo) {
        return NULL;
    }
    p = gbm_bo_map(bo, 0, 0, SW, SH, GBM_BO_TRANSFER_WRITE, &stride, &map);
    if (!p) {
        gbm_bo_destroy(bo);
        return NULL;
    }
    for (int y = 0; y < SH; y++) {
        for (int x = 0; x < SW; x++) {
            *(uint32_t *)(p + (size_t)y * stride + (size_t)x * 4) =
                x < edge ? 0xff0000 : 0x0000ff;
        }
    }
    gbm_bo_unmap(bo, map);
    return bo;
}

/* Within `tol` per channel. */
static bool close_to(uint32_t a, uint32_t b, int tol)
{
    for (int sh = 0; sh < 24; sh += 8) {
        int d = (int)((a >> sh) & 0xff) - (int)((b >> sh) & 0xff);

        if (d < -tol || d > tol) {
            return false;
        }
    }
    return true;
}

/*
 * PICTURE ENHANCEMENT (docs/VIEWER-ENHANCE.md) on this GPU: each upscaler
 * draws the quadrant picture 2x into the letterboxed target; flat areas stay
 * their colour (sharpening only acts on edges), the bars stay black, the
 * pass that ran is the one asked for.  Then frame generation: two frames
 * whose red/blue edge moves 8 pixels; the halfway picture must put the edge
 * halfway (a cross-fade would make both sides purple).
 */
static void test_enhance(struct nb_gs *g, struct gbm_device *dev)
{
    static const int ups[3] = { NB_UPSCALE_FSR, NB_UPSCALE_NIS,
                                NB_UPSCALE_CAS };
    const char *why = NULL;

    if (!nb_gs_enhance_ok(g, &why)) {
        printf("test_gpuscale: enhancement SKIPPED: %s\n", why ? why : "?");
        return;
    }
    for (int i = 0; i < 3; i++) {
        struct nb_gs_draw d = {
            .src_key = 3, .target = 1, .dx = 160, .dy = 0, .dw = 320,
            .dh = 180, .sx = 0, .sy = 0, .sw = SW, .sh = SH,
            .upscale = ups[i], .sharp = 50,
        };
        unsigned us = 0;
        int r = nb_gs_draw(g, &d, 1000000, &us);
        int used = nb_gs_enh_status(g, &why);

        CHECK(r == 0, "%s draw (%d)", nb_upscale_label(ups[i]), r);
        CHECK(used == ups[i], "%s ran (got %s: %s)", nb_upscale_label(ups[i]),
              nb_upscale_label(used), why ? why : "");
        CHECK(px(g, 1, 10, 90) == 0 && px(g, 1, 630, 90) == 0,
              "%s: bars black", nb_upscale_label(ups[i]));
        CHECK(close_to(px(g, 1, 200, 20), 0xff0000, 2), "%s: red %06x",
              nb_upscale_label(ups[i]), px(g, 1, 200, 20));
        CHECK(close_to(px(g, 1, 200, 160), 0x00ff00, 2), "%s: green %06x",
              nb_upscale_label(ups[i]), px(g, 1, 200, 160));
        CHECK(close_to(px(g, 1, 440, 90), 0x0000ff, 2), "%s: blue %06x",
              nb_upscale_label(ups[i]), px(g, 1, 440, 90));
        printf("test_gpuscale: %s 160x90 -> 320x180: %u us\n",
               nb_upscale_label(ups[i]), us);
    }
    /* Smaller than the source: FSR steps aside, and says why. */
    {
        struct nb_gs_draw d = {
            .src_key = 3, .target = 2, .dx = 0, .dy = 0, .dw = 80, .dh = 45,
            .sx = 0, .sy = 0, .sw = SW, .sh = SH, .upscale = NB_UPSCALE_FSR,
        };

        CHECK(nb_gs_draw(g, &d, 1000000, NULL) == 0, "FSR downscale draw");
        CHECK(nb_gs_enh_status(g, &why) == NB_UPSCALE_OFF && why,
              "FSR downscale -> plain pass");
    }
    /* Frame generation. */
    {
        struct gbm_bo *a = edge_src(dev, 64), *b = edge_src(dev, 72);
        struct nb_gs_draw d = {
            .src_key = NB_GS_SRC_FG, .target = 0, .dx = 0, .dy = 0,
            .dw = SW, .dh = SH, .nearest = true, .fg_t = 0.5f,
        };
        unsigned us = 0;

        CHECK(a && b, "edge sources");
        if (!a || !b) {
            return;
        }
        CHECK(nb_gs_src(g, 6, gbm_bo_get_fd(a), SW, SH, gbm_bo_get_stride(a),
                        0, GBM_FORMAT_XRGB8888, 0) >= 0, "import frame A");
        CHECK(nb_gs_src(g, 7, gbm_bo_get_fd(b), SW, SH, gbm_bo_get_stride(b),
                        0, GBM_FORMAT_XRGB8888, 0) >= 0, "import frame B");
        nb_gs_fg_reset(g);
        CHECK(nb_gs_fg_push(g, 6, 0, 0, SW, SH, 1000000, &us) == 0,
              "first frame: no pair");
        CHECK(nb_gs_fg_push(g, 7, 0, 0, SW, SH, 1000000, &us) == 1,
              "second frame: a pair");
        printf("test_gpuscale: frame generation push (pyramid + flow) "
               "160x90: %u us\n", us);
        CHECK(nb_gs_draw(g, &d, 1000000, &us) == 0, "halfway picture");
        CHECK(close_to(px(g, 0, 20, 45), 0xff0000, 4), "far left red %06x",
              px(g, 0, 20, 45));
        CHECK(close_to(px(g, 0, 140, 45), 0x0000ff, 4), "far right blue %06x",
              px(g, 0, 140, 45));
        CHECK(close_to(px(g, 0, 66, 45), 0xff0000, 40),
              "x=66 is red at t=1/2 (the edge moved to 68): %06x",
              px(g, 0, 66, 45));
        CHECK(close_to(px(g, 0, 70, 45), 0x0000ff, 40),
              "x=70 is blue at t=1/2: %06x", px(g, 0, 70, 45));
        d.fg_t = 1.0f;
        CHECK(nb_gs_draw(g, &d, 1000000, NULL) == 0 &&
              px(g, 0, 70, 45) == 0xff0000 && px(g, 0, 72, 45) == 0x0000ff,
              "t=1 is frame B exactly");
        d.fg_t = 0.0f;
        CHECK(nb_gs_draw(g, &d, 1000000, NULL) == 0 &&
              px(g, 0, 63, 45) == 0xff0000 && px(g, 0, 64, 45) == 0x0000ff,
              "t=0 is frame A exactly");
        /* the generated picture through an upscaler */
        d.fg_t = 0.5f;
        d.upscale = NB_UPSCALE_FSR;
        d.sharp = 50;
        d.dw = 2 * SW > TW ? TW : 2 * SW;
        d.dh = 2 * SH > TH ? TH : 2 * SH;
        CHECK(nb_gs_draw(g, &d, 1000000, NULL) == 0 &&
              nb_gs_enh_status(g, NULL) == NB_UPSCALE_FSR,
              "generated picture upscaled with FSR");
        nb_gs_src_drop(g, 6);
        nb_gs_src_drop(g, 7);
        gbm_bo_destroy(a);
        gbm_bo_destroy(b);
    }
}

/*
 * TEST_ENH_BENCH=1: what the enhancement passes cost at real sizes, a
 * 1920x1080 guest frame drawn at 3840x2160 (median of 60 after a warm-up,
 * the pass waited for as the viewer does).  Numbers for
 * docs/VIEWER-ENHANCE.md; not a check.
 */
static unsigned med(unsigned *v, int n)
{
    for (int i = 1; i < n; i++) {
        unsigned k = v[i];
        int j = i - 1;

        while (j >= 0 && v[j] > k) {
            v[j + 1] = v[j];
            j--;
        }
        v[j + 1] = k;
    }
    return v[n / 2];
}

static unsigned wall_us(const struct timespec *a)
{
    struct timespec b;

    clock_gettime(CLOCK_MONOTONIC, &b);
    return (unsigned)((b.tv_sec - a->tv_sec) * 1000000 +
                      (b.tv_nsec - a->tv_nsec) / 1000);
}

static void enh_bench(struct nb_gs *g, struct gbm_device *dev)
{
    enum { BW = 1920, BH = 1080, OW = 3840, OH = 2160, N = 60 };
    struct gbm_bo *bo[2];
    struct nb_gs_target tg[NB_GS_TARGETS];
    char why[200] = "";
    unsigned v[N];

    for (int k = 0; k < 2; k++) {
        void *map = NULL;
        uint32_t stride;
        uint8_t *p;

        bo[k] = gbm_bo_create(dev, BW, BH, GBM_FORMAT_XRGB8888,
                              GBM_BO_USE_LINEAR);
        p = bo[k] ? gbm_bo_map(bo[k], 0, 0, BW, BH, GBM_BO_TRANSFER_WRITE,
                               &stride, &map) : NULL;
        if (!p) {
            printf("test_gpuscale: bench SKIPPED (no 1080p buffer)\n");
            return;
        }
        for (int y = 0; y < BH; y++) {
            for (int x = 0; x < BW; x++) {
                /* a texture moving 6 px per frame */
                int u = x + 6 * k;

                *(uint32_t *)(p + (size_t)y * stride + (size_t)x * 4) =
                    (uint32_t)(((u * 7) ^ (y * 3)) & 0xff) << 16 |
                    (uint32_t)((u + y) & 0xff) << 8 | (uint32_t)(y & 0xff);
            }
        }
        gbm_bo_unmap(bo[k], map);
        nb_gs_src(g, 10 + k, gbm_bo_get_fd(bo[k]), BW, BH,
                  gbm_bo_get_stride(bo[k]), 0, GBM_FORMAT_XRGB8888, 0);
    }
    if (nb_gs_targets(g, OW, OH, NULL, NULL, NULL, NULL, 0, tg, why,
                      sizeof(why)) != NB_GS_TARGETS) {
        printf("test_gpuscale: bench SKIPPED (%s)\n", why);
        return;
    }
    for (int up = NB_UPSCALE_OFF; up < NB_UPSCALE_COUNT; up++) {
        struct nb_gs_draw d = {
            .src_key = 10, .dw = OW, .dh = OH, .sw = BW, .sh = BH,
            .upscale = up, .sharp = 50,
        };

        for (int i = 0; i < N + 5; i++) {
            struct timespec a;

            d.target = i % NB_GS_TARGETS;
            clock_gettime(CLOCK_MONOTONIC, &a);
            nb_gs_draw(g, &d, 1000000, NULL);
            if (i >= 5) {
                v[i - 5] = wall_us(&a);
            }
        }
        printf("test_gpuscale: bench %dx%d -> %dx%d %-18s %5u us\n", BW, BH,
               OW, OH, up ? nb_upscale_label(up) : "plain (bilinear)",
               med(v, N));
    }
    for (int i = 0; i < N + 5; i++) {
        struct timespec a;

        clock_gettime(CLOCK_MONOTONIC, &a);
        nb_gs_fg_push(g, 10 + (i & 1), 0, 0, BW, BH, 1000000, NULL);
        if (i >= 5) {
            v[i - 5] = wall_us(&a);
        }
    }
    printf("test_gpuscale: bench frame generation: push %dx%d (copy, "
           "pyramid, flow) %u us\n", BW, BH, med(v, N));
    {
        struct nb_gs_draw d = {
            .src_key = NB_GS_SRC_FG, .dw = OW, .dh = OH, .fg_t = 0.5f,
        };

        for (int up = NB_UPSCALE_OFF; up <= NB_UPSCALE_FSR; up++) {
            d.upscale = up;
            d.sharp = 50;
            for (int i = 0; i < N + 5; i++) {
                struct timespec a;

                d.target = i % NB_GS_TARGETS;
                clock_gettime(CLOCK_MONOTONIC, &a);
                nb_gs_draw(g, &d, 1000000, NULL);
                if (i >= 5) {
                    v[i - 5] = wall_us(&a);
                }
            }
            printf("test_gpuscale: bench frame generation: generated "
                   "picture at %dx%d, %s: %u us\n", OW, OH,
                   up ? "FSR 1" : "plain", med(v, N));
        }
    }
    nb_gs_src_drop(g, 10);
    nb_gs_src_drop(g, 11);
    gbm_bo_destroy(bo[0]);
    gbm_bo_destroy(bo[1]);
}

int main(void)
{
    char why[256] = "";
    const char *node = getenv("TEST_RENDER_NODE");
    struct gbm_device *dev;
    struct gbm_bo *src;
    struct nb_gs *g;
    struct nb_gs_target tg[NB_GS_TARGETS];
    uint32_t stride;
    void *map = NULL;
    uint8_t *p;
    int fd, sfd;
    unsigned gpu_us = 0;

    /* The priority knob's words: no GPU needed. */
    {
        bool bad = true;

        CHECK(nb_gs_prio_parse(NULL, &bad) == NB_GS_PRIO_HIGH && !bad,
              "default is high");
        CHECK(nb_gs_prio_parse("", &bad) == NB_GS_PRIO_HIGH && !bad, "empty");
        CHECK(nb_gs_prio_parse("off", &bad) == NB_GS_PRIO_MEDIUM && !bad, "off");
        CHECK(nb_gs_prio_parse("MEDIUM", &bad) == NB_GS_PRIO_MEDIUM && !bad,
              "medium");
        CHECK(nb_gs_prio_parse("realtime", &bad) == NB_GS_PRIO_REALTIME &&
              !bad, "realtime");
        CHECK(nb_gs_prio_parse("bogus", &bad) == NB_GS_PRIO_HIGH && bad,
              "unknown word -> high, reported");
        CHECK(!strcmp(nb_gs_prio_name(NB_GS_PRIO_UNKNOWN), "unknown"), "name");
    }
    if (!node) {
        node = "/dev/dri/renderD128";
    }
    fd = open(node, O_RDWR | O_CLOEXEC);
    if (fd < 0 || !(dev = gbm_create_device(fd))) {
        printf("test_gpuscale: SKIP (no %s)\n", node);
        return 77;
    }
    {
        uint64_t lin = 0;

        src = gbm_bo_create_with_modifiers(dev, SW, SH, GBM_FORMAT_XRGB8888,
                                           &lin, 1);
    }
    if (!src) {
        src = gbm_bo_create(dev, SW, SH, GBM_FORMAT_XRGB8888,
                            GBM_BO_USE_LINEAR);
    }
    if (!src) {
        printf("test_gpuscale: SKIP (no LINEAR gbm buffer)\n");
        return 77;
    }
    p = gbm_bo_map(src, 0, 0, SW, SH, GBM_BO_TRANSFER_WRITE, &stride, &map);
    if (!p) {
        printf("test_gpuscale: SKIP (gbm_bo_map unsupported)\n");
        return 77;
    }
    for (int y = 0; y < SH; y++) {
        for (int x = 0; x < SW; x++) {
            uint32_t c = x >= SW / 2 ? 0x0000ff : y < SH / 2 ? 0xff0000
                                                             : 0x00ff00;
            *(uint32_t *)(p + (size_t)y * stride + (size_t)x * 4) = c;
        }
    }
    gbm_bo_unmap(src, map);

    g = nb_gs_open(node, 0, GBM_FORMAT_XRGB8888, 0, why, sizeof(why));
    if (!g) {
        printf("test_gpuscale: SKIP (%s)\n", why);
        return 77;
    }
    CHECK(nb_gs_targets(g, TW, TH, NULL, NULL, NULL, NULL, 0, tg, why, sizeof(why)) ==
          NB_GS_TARGETS, "targets: %s", why);
    printf("test_gpuscale: target %ux%u %.4s modifier 0x%016llx on %s, "
           "context priority requested %s, granted %s\n",
           tg[0].w, tg[0].h, (const char *)&tg[0].fourcc,
           (unsigned long long)tg[0].modifier, nb_gs_node(g),
           nb_gs_prio_name(nb_gs_prio_requested(g)),
           nb_gs_prio_name(nb_gs_prio_granted(g)));
    CHECK(tg[0].w == TW && tg[0].h == TH, "target size");
    sfd = gbm_bo_get_fd(src);
    CHECK(nb_gs_src(g, 3, sfd, SW, SH, gbm_bo_get_stride(src), 0,
                    GBM_FORMAT_XRGB8888, 0) >= 0, "import source");
    CHECK(nb_gs_src(g, 3, -1, SW, SH, 0, 0, 0, 0) >= 0, "source cached");

    /* Fit: 160x90 in 640x180 is 320x180 at x = 160. */
    {
        struct nb_gs_draw d = {
            .src_key = 3, .target = 1, .dx = 160, .dy = 0, .dw = 320,
            .dh = 180, .sx = 0, .sy = 0, .sw = SW, .sh = SH, .nearest = true,
        };

        CHECK(nb_gs_draw(g, &d, 1000000, &gpu_us) == 0, "draw");
        CHECK(px(g, 1, 10, 90) == 0, "left bar black %06x", px(g, 1, 10, 90));
        CHECK(px(g, 1, 630, 90) == 0, "right bar black %06x",
              px(g, 1, 630, 90));
        CHECK(px(g, 1, 200, 20) == 0xff0000, "top-left red %06x",
              px(g, 1, 200, 20));
        CHECK(px(g, 1, 200, 160) == 0x00ff00,
              "bottom-left green (not flipped) %06x", px(g, 1, 200, 160));
        CHECK(px(g, 1, 440, 90) == 0x0000ff, "right blue %06x",
              px(g, 1, 440, 90));
        CHECK(px(g, 1, 160, 0) == 0xff0000 && px(g, 1, 159, 0) == 0,
              "picture edge at x = 160");
    }
    /* A source rectangle (a crop): only the right half, over everything. */
    {
        struct nb_gs_draw d = {
            .src_key = 3, .target = 2, .dx = 0, .dy = 0, .dw = TW, .dh = TH,
            .sx = SW / 2, .sy = 0, .sw = SW / 2, .sh = SH, .nearest = true,
        };

        CHECK(nb_gs_draw(g, &d, 1000000, NULL) == 0, "draw crop");
        CHECK(px(g, 2, 0, 0) == 0x0000ff && px(g, 2, TW - 1, TH - 1) == 0x0000ff,
              "crop shows only blue");
    }
    /* Linear filtering blends across the red/blue edge; nearest does not. */
    {
        struct nb_gs_draw d = {
            .src_key = 3, .target = 0, .dx = 0, .dy = 0, .dw = TW, .dh = TH,
            .sx = 0, .sy = 0, .sw = SW, .sh = SH, .nearest = false,
        };
        uint32_t c;

        CHECK(nb_gs_draw(g, &d, 1000000, NULL) == 0, "draw linear");
        c = px(g, 0, TW / 2, 20);
        CHECK(c != 0xff0000 && c != 0x0000ff, "linear blends at the edge %06x",
              c);
    }
    /* Read-back targets: the same pass, read into XRGB8888 memory (B, G,
     * R, X bytes), rows top-down. */
    {
        static uint32_t mem[TW * TH];
        struct nb_gs_draw d = {
            .src_key = 3, .target = 1, .dx = 160, .dy = 0, .dw = 320,
            .dh = 180, .sx = 0, .sy = 0, .sw = SW, .sh = SH, .nearest = true,
        };

        CHECK(nb_gs_targets_readback(g, TW, TH, why, sizeof(why)) ==
              NB_GS_TARGETS, "read-back targets: %s", why);
        CHECK(nb_gs_draw(g, &d, 1000000, NULL) == 0, "draw read-back");
        CHECK(nb_gs_readback(g, 1, mem) == 0, "read back");
        CHECK((mem[90 * TW + 10] & 0xffffff) == 0, "bar black %08x",
              mem[90 * TW + 10]);
        CHECK((mem[20 * TW + 200] & 0xffffff) == 0xff0000,
              "top-left red as XRGB %08x", mem[20 * TW + 200]);
        CHECK((mem[160 * TW + 200] & 0xffffff) == 0x00ff00,
              "bottom-left green %08x", mem[160 * TW + 200]);
        CHECK((mem[90 * TW + 440] & 0xffffff) == 0x0000ff,
              "right blue as XRGB %08x", mem[90 * TW + 440]);
        CHECK(nb_gs_targets(g, TW, TH, NULL, NULL, NULL, NULL, 0, tg, why, sizeof(why)) ==
              NB_GS_TARGETS, "back to dma-buf targets");
    }
    /* A cursor image: LINEAR ARGB8888 drawn 1:1 with its alpha kept and read
     * back as ARGB8888 bytes, exactly; without .alpha it comes out opaque. */
    {
        static uint32_t mem[64 * 64];
        struct gbm_bo *cur = gbm_bo_create(dev, 64, 64, GBM_FORMAT_ARGB8888,
                                           GBM_BO_USE_LINEAR);
        struct nb_gs_draw d = {
            .src_key = 5, .target = 0, .dw = 64, .dh = 64, .sw = 64, .sh = 64,
            .nearest = true, .alpha = true,
        };
        void *cmap = NULL;
        uint32_t cst;
        uint8_t *c;

        CHECK(cur != NULL, "LINEAR ARGB8888 cursor buffer");
        c = gbm_bo_map(cur, 0, 0, 64, 64, GBM_BO_TRANSFER_WRITE, &cst, &cmap);
        CHECK(c != NULL, "map the cursor buffer");
        for (int y = 0; y < 64; y++) {
            for (int x = 0; x < 64; x++) {
                /* left: transparent; right top: opaque white; right bottom:
                 * half-transparent red, premultiplied. */
                uint32_t v = x < 32 ? 0x00000000u
                           : y < 32 ? 0xffffffffu : 0x80800000u;
                *(uint32_t *)(c + (size_t)y * cst + (size_t)x * 4) = v;
            }
        }
        gbm_bo_unmap(cur, cmap);
        CHECK(nb_gs_targets_readback(g, 64, 64, why, sizeof(why)) ==
              NB_GS_TARGETS, "cursor read-back targets: %s", why);
        CHECK(nb_gs_src(g, 5, gbm_bo_get_fd(cur), 64, 64,
                        gbm_bo_get_stride(cur), 0, GBM_FORMAT_ARGB8888, 0) >= 0,
              "import the cursor");
        CHECK(nb_gs_draw(g, &d, 1000000, NULL) == 0, "draw the cursor");
        CHECK(nb_gs_readback(g, 0, mem) == 0, "read the cursor back");
        CHECK(mem[10 * 64 + 10] == 0x00000000u, "transparent stays %08x",
              mem[10 * 64 + 10]);
        CHECK(mem[10 * 64 + 50] == 0xffffffffu, "opaque white %08x",
              mem[10 * 64 + 50]);
        CHECK(mem[50 * 64 + 50] == 0x80800000u, "half red, alpha kept %08x",
              mem[50 * 64 + 50]);
        d.alpha = false;
        CHECK(nb_gs_draw(g, &d, 1000000, NULL) == 0, "draw opaque");
        CHECK(nb_gs_readback(g, 0, mem) == 0, "read opaque back");
        CHECK(mem[10 * 64 + 10] >> 24 == 0xff, "opaque without .alpha %08x",
              mem[10 * 64 + 10]);
        nb_gs_src_drop(g, 5);
        gbm_bo_destroy(cur);
        CHECK(nb_gs_targets(g, TW, TH, NULL, NULL, NULL, NULL, 0, tg, why,
                            sizeof(why)) == NB_GS_TARGETS,
              "back to dma-buf targets after the cursor");
    }
    test_enhance(g, dev);
    if (getenv("TEST_ENH_BENCH") && nb_gs_enhance_ok(g, NULL)) {
        enh_bench(g, dev);
    }
    CHECK(nb_gs_targets(g, TW, TH, NULL, NULL, NULL, NULL, 0, tg, why,
                        sizeof(why)) == NB_GS_TARGETS,
          "targets after the enhancement tests");
    /*
     * TEST_GPUSCALE_BENCH=WxH: the per-frame cost at a real output size (a
     * 1600x900-class source stretched over the output), 500 draws, of the
     * gpu-copy level (the pass into dma-buf targets) and of the readback
     * level (the pass plus the copy into memory).
     */
    if (getenv("TEST_GPUSCALE_BENCH")) {
        unsigned bw = 0, bh = 0;

        if (sscanf(getenv("TEST_GPUSCALE_BENCH"), "%ux%u", &bw, &bh) == 2) {
            uint32_t *mem = malloc((size_t)bw * bh * 4);

            if (nb_gs_targets(g, bw, bh, NULL, NULL, NULL, NULL, 0, tg, why,
                              sizeof(why)) == NB_GS_TARGETS) {
                bench(g, "gpu-copy", bw, bh, NULL);
            }
            if (mem && nb_gs_targets_readback(g, bw, bh, why, sizeof(why)) ==
                           NB_GS_TARGETS) {
                bench(g, "readback", bw, bh, mem);
            }
            free(mem);
        }
    }
    nb_gs_src_drop(g, 3);
    nb_gs_close(g);
    close(sfd);
    gbm_bo_destroy(src);
    gbm_device_destroy(dev);
    printf("test_gpuscale: %d checks, %d failed (GPU pass %u us)\n", checks,
           fails, gpu_us);
    return fails ? 1 : 0;
}
