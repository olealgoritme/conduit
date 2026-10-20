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
