/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * test_gpusel.c -- the viewer's GPU (nb_gpusel.h): classification, dgpu /
 * igpu / auto selection, and the sysfs scan against a fake tree.
 */
#include <stdio.h>
#include <stdlib.h>
#include <sys/stat.h>

#include "nb_gpusel.h"

static int checks, fails;
#define CHECK(c, ...) do { checks++; if (!(c)) { fails++; \
        printf("FAIL %s:%d: ", __FILE__, __LINE__); printf(__VA_ARGS__); \
        printf("\n"); } } while (0)

#define GIB (1ull << 30)

/* This host today: the AMD iGPU booted as VGA and sorts second by name in
 * one boot, first in another; the NVIDIA card has 32 GiB. */
static const struct nb_gpu_node host[2] = {
    { "/dev/dri/renderD128", "0000:01:00.0", NB_PCI_NVIDIA, false, 0 },
    { "/dev/dri/renderD129", "0000:0e:00.0", NB_PCI_AMD, true, 512ull << 20 },
};

static void classify(void)
{
    struct nb_gpu_node g = { "/dev/dri/renderD128", "0000:00:02.0",
                             NB_PCI_INTEL, true, 0 };

    CHECK(nb_gpu_classify(&host[0]) == NB_GPU_DISCRETE, "nvidia");
    CHECK(nb_gpu_classify(&host[1]) == NB_GPU_INTEGRATED, "amd apu");
    CHECK(nb_gpu_classify(&g) == NB_GPU_INTEGRATED, "intel igpu slot");
    snprintf(g.slot, sizeof(g.slot), "0000:03:00.0");
    CHECK(nb_gpu_classify(&g) == NB_GPU_DISCRETE, "intel arc");
    g.vendor = NB_PCI_AMD;
    g.vram = 16 * GIB;
    CHECK(nb_gpu_classify(&g) == NB_GPU_DISCRETE, "amd dgpu");
    g.vram = 0;
    CHECK(nb_gpu_classify(&g) == NB_GPU_UNKNOWN, "amd, vram unknown");
    g.vendor = 0x1af4;
    CHECK(nb_gpu_classify(&g) == NB_GPU_UNKNOWN, "virtio");
}

static void pick(void)
{
    struct nb_gpu_node swapped[2] = { host[1], host[0] };
    struct nb_gpu_node two[3] = {
        { "/dev/dri/renderD128", "0000:0a:00.0", NB_PCI_AMD, false, 8 * GIB },
        { "/dev/dri/renderD129", "0000:01:00.0", NB_PCI_NVIDIA, false, 0 },
        { "/dev/dri/renderD130", "0000:00:02.0", NB_PCI_INTEL, true, 0 },
    };
    char path[64], why[200];

    CHECK(nb_gpu_pick(host, 2, true) == 0, "dgpu");
    CHECK(nb_gpu_pick(host, 2, false) == 1, "igpu");
    /* Card numbers swapped between boots: same GPUs chosen. */
    CHECK(!strcmp(swapped[nb_gpu_pick(swapped, 2, true)].path,
                  "/dev/dri/renderD128"), "dgpu after a swap");
    CHECK(!strcmp(swapped[nb_gpu_pick(swapped, 2, false)].path,
                  "/dev/dri/renderD129"), "igpu after a swap");
    /* Two discrete: the larger (NVIDIA reports no VRAM size: the AMD one
     * with 8 GiB is larger than "unknown"). */
    CHECK(nb_gpu_pick(two, 3, true) == 0, "largest discrete");
    CHECK(nb_gpu_pick(two, 3, false) == 2, "intel igpu");
    CHECK(nb_gpu_pick(host, 1, false) == -1, "no igpu");

    CHECK(nb_gpusel_resolve("dgpu", host, 2, path, sizeof(path), why,
                            sizeof(why)) &&
          !strcmp(path, "/dev/dri/renderD128") && strstr(why, "NVIDIA"),
          "resolve dgpu: %s", why);
    CHECK(nb_gpusel_resolve("igpu", host, 2, path, sizeof(path), why,
                            sizeof(why)) &&
          !strcmp(path, "/dev/dri/renderD129") && strstr(why, "boot VGA"),
          "resolve igpu: %s", why);
    CHECK(nb_gpusel_resolve("auto", host, 2, path, sizeof(path), why,
                            sizeof(why)) && !path[0], "auto leaves it open");
    CHECK(nb_gpusel_resolve("/dev/dri/renderD129", host, 2, path,
                            sizeof(path), why, sizeof(why)) &&
          !strcmp(path, "/dev/dri/renderD129"), "a path");
    CHECK(!nb_gpusel_resolve("igpu", host, 1, path, sizeof(path), why,
                             sizeof(why)) && strstr(why, "no integrated"),
          "missing igpu: %s", why);
    CHECK(!nb_gpusel_resolve("fast", host, 2, path, sizeof(path), why,
                             sizeof(why)), "bad value");
}

static void order(void)
{
    int o[2];

    /* An NVIDIA frame: the NVIDIA GPU first, whichever node number it has. */
    nb_gpu_order(host, 2, 0x0300000000606014ull, o);
    CHECK(o[0] == 0 && o[1] == 1, "nvidia frame %d %d", o[0], o[1]);
    {
        struct nb_gpu_node swapped[2] = { host[1], host[0] };

        nb_gpu_order(swapped, 2, 0x0300000000606014ull, o);
        CHECK(o[0] == 1 && o[1] == 0, "nvidia frame, swapped");
    }
    nb_gpu_order(host, 2, 0x0200000000401b03ull, o);
    CHECK(o[0] == 1 && o[1] == 0, "amd frame");
    /* LINEAR / implicit: scan order. */
    nb_gpu_order(host, 2, 0, o);
    CHECK(o[0] == 0 && o[1] == 1, "linear");
    nb_gpu_order(host, 2, 0x00ffffffffffffffull, o);
    CHECK(o[0] == 0 && o[1] == 1, "implicit");
    CHECK(nb_mod_pci_vendor(0x0100000000000001ull) == NB_PCI_INTEL, "intel");
}

static void put(const char *path, const char *val)
{
    FILE *f = fopen(path, "w");

    if (f) {
        fputs(val, f);
        fclose(f);
    }
}

static void scan(void)
{
    char root[] = "/tmp/test-gpusel-XXXXXX", p[256], q[512];
    struct nb_gpu_node g[NB_GPU_MAX];
    int n;

    if (!mkdtemp(root)) {
        return;
    }
    snprintf(p, sizeof(p), "%s/sys", root); mkdir(p, 0700);
    snprintf(p, sizeof(p), "%s/sys/class", root); mkdir(p, 0700);
    snprintf(p, sizeof(p), "%s/sys/class/drm", root); mkdir(p, 0700);
    snprintf(p, sizeof(p), "%s/sys/devices", root); mkdir(p, 0700);
    for (int i = 0; i < 2; i++) {
        snprintf(p, sizeof(p), "%s/sys/devices/%s", root, host[i].slot);
        mkdir(p, 0700);
        snprintf(q, sizeof(q), "%s/vendor", p);
        put(q, i == 0 ? "0x10de\n" : "0x1002\n");
        snprintf(q, sizeof(q), "%s/boot_vga", p);
        put(q, i == 0 ? "0\n" : "1\n");
        if (i == 1) {
            snprintf(q, sizeof(q), "%s/mem_info_vram_total", p);
            put(q, "536870912\n");
        }
        snprintf(q, sizeof(q), "%s/sys/class/drm/renderD%d", root, 128 + i);
        mkdir(q, 0700);
        snprintf(q, sizeof(q), "%s/sys/class/drm/renderD%d/device", root,
                 128 + i);
        if (symlink(p, q) != 0) {
            CHECK(0, "symlink");
        }
    }
    snprintf(q, sizeof(q), "%s/sys/class/drm/card0", root);
    mkdir(q, 0700);
    n = nb_gpusel_scan(root, g, NB_GPU_MAX);
    CHECK(n == 2, "scan found %d", n);
    if (n == 2) {
        CHECK(!strcmp(g[0].path, "/dev/dri/renderD128") &&
              g[0].vendor == NB_PCI_NVIDIA && !g[0].boot_vga &&
              !strcmp(g[0].slot, "0000:01:00.0"), "node 0 %s %x %s",
              g[0].path, g[0].vendor, g[0].slot);
        CHECK(g[1].vendor == NB_PCI_AMD && g[1].boot_vga &&
              g[1].vram == 536870912ull, "node 1");
        CHECK(nb_gpu_pick(g, n, false) == 1 && nb_gpu_pick(g, n, true) == 0,
              "picks from the scan");
    }
    snprintf(p, sizeof(p), "rm -rf '%s'", root);
    if (system(p) != 0) {
        CHECK(0, "cleanup");
    }
}

int main(void)
{
    classify();
    pick();
    order();
    scan();
    printf("test_gpusel: %d checks, %d failed\n", checks, fails);
    return fails ? 1 : 0;
}
