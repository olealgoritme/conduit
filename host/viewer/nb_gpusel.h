/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * nb_gpusel.h -- which GPU the viewer's GPU pass (nb_gpuscale.h) runs on.
 *
 * --gpu (the `viewer.gpu` setting):
 *   auto   the GPU that can import the guest's frames, the one whose PCI
 *          vendor matches the frame's modifier vendor first (the GPU that
 *          produced it), then any other that imports it;
 *   dgpu   the discrete GPU (the largest by video memory);
 *   igpu   the integrated one;
 *   PATH   that render node.
 * dgpu/igpu are resolved from PCI facts in sysfs (vendor, slot, video
 * memory), never from card numbers, which can swap between boots (and
 * boot_vga only says which GPU the firmware lit, not which is which).
 *
 * Classification, per render node:
 *   NVIDIA                                   discrete
 *   Intel at 0000:00:02.0 (the iGPU slot)    integrated, elsewhere discrete
 *   AMD with under 4 GiB of VRAM (an APU's carve-out)  integrated,
 *                 more                       discrete, unknown: unknown
 *   anything else                            unknown
 *
 * The selection is pure (test/test_gpusel.c); nb_gpusel_scan() reads sysfs.
 */
#ifndef NB_GPUSEL_H
#define NB_GPUSEL_H

#include <dirent.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define NB_PCI_NVIDIA 0x10de
#define NB_PCI_AMD    0x1002
#define NB_PCI_INTEL  0x8086

enum nb_gpu_kind { NB_GPU_UNKNOWN = 0, NB_GPU_DISCRETE, NB_GPU_INTEGRATED };

struct nb_gpu_node {
    char     path[64];          /* /dev/dri/renderD128                       */
    char     slot[16];          /* PCI address, 0000:01:00.0                 */
    unsigned vendor;            /* PCI vendor id                             */
    bool     boot_vga;
    uint64_t vram;              /* bytes, 0 unknown                          */
};

#define NB_GPU_MAX 8

static inline enum nb_gpu_kind nb_gpu_classify(const struct nb_gpu_node *g)
{
    switch (g->vendor) {
    case NB_PCI_NVIDIA:
        return NB_GPU_DISCRETE;
    case NB_PCI_INTEL:
        return !strcmp(g->slot, "0000:00:02.0") ? NB_GPU_INTEGRATED
                                                : NB_GPU_DISCRETE;
    case NB_PCI_AMD:
        if (!g->vram) {
            return NB_GPU_UNKNOWN;
        }
        return g->vram < (4ull << 30) ? NB_GPU_INTEGRATED : NB_GPU_DISCRETE;
    default:
        return NB_GPU_UNKNOWN;
    }
}

static inline const char *nb_gpu_kind_name(enum nb_gpu_kind k)
{
    return k == NB_GPU_DISCRETE ? "discrete"
         : k == NB_GPU_INTEGRATED ? "integrated" : "unknown kind";
}

static inline const char *nb_gpu_vendor_name(unsigned v)
{
    return v == NB_PCI_NVIDIA ? "NVIDIA" : v == NB_PCI_AMD ? "AMD"
         : v == NB_PCI_INTEL ? "Intel" : "other";
}

/* The PCI vendor whose GPUs write this modifier, 0 for LINEAR/implicit/other
 * (DRM_FORMAT_MOD_VENDOR_*: 0x01 Intel, 0x02 AMD, 0x03 NVIDIA). */
static inline unsigned nb_mod_pci_vendor(uint64_t mod)
{
    if (mod == 0x00ffffffffffffffull) {
        return 0;
    }
    switch (mod >> 56) {
    case 0x01: return NB_PCI_INTEL;
    case 0x02: return NB_PCI_AMD;
    case 0x03: return NB_PCI_NVIDIA;
    default:   return 0;
    }
}

/*
 * --gpu dgpu / igpu: the index of the node to use, -1 if there is none.
 * dgpu: the discrete GPU with the most video memory (NVIDIA first on a tie);
 * igpu: the first integrated one.
 */
static inline int nb_gpu_pick(const struct nb_gpu_node *g, int n, bool dgpu)
{
    int best = -1;

    for (int i = 0; i < n; i++) {
        enum nb_gpu_kind k = nb_gpu_classify(&g[i]);

        if (dgpu ? k != NB_GPU_DISCRETE : k != NB_GPU_INTEGRATED) {
            continue;
        }
        if (!dgpu) {
            return i;
        }
        if (best < 0 || g[i].vram > g[best].vram ||
            (g[i].vram == g[best].vram && g[i].vendor == NB_PCI_NVIDIA &&
             g[best].vendor != NB_PCI_NVIDIA)) {
            best = i;
        }
    }
    return best;
}

/*
 * --gpu auto: the order in which to try the nodes for a frame with this
 * modifier.  The producer's vendor first (its GPU imports its own layout
 * natively, and the frame's memory is local to it), the others after, each
 * group in scan order.  Writes n indices to order[].
 */
static inline void nb_gpu_order(const struct nb_gpu_node *g, int n,
                                uint64_t mod, int *order)
{
    unsigned v = nb_mod_pci_vendor(mod);
    int k = 0;

    for (int pass = 0; pass < 2; pass++) {
        for (int i = 0; i < n; i++) {
            bool match = v && g[i].vendor == v;

            if (pass == 0 ? match : !match) {
                order[k++] = i;
            }
        }
    }
}

/* ── sysfs ──────────────────────────────────────────────────────────────── */

static inline unsigned long long nb_gpusel_read_num(const char *path)
{
    char b[64];
    FILE *f = fopen(path, "r");
    unsigned long long v = 0;

    if (f) {
        if (fgets(b, sizeof(b), f)) {
            v = strtoull(b, NULL, 0);
        }
        fclose(f);
    }
    return v;
}

static inline int nb_gpusel_cmp(const void *a, const void *b)
{
    return strcmp(((const struct nb_gpu_node *)a)->path,
                  ((const struct nb_gpu_node *)b)->path);
}

/* The render nodes, from /sys/class/drm (root: for tests, NULL = "/"). */
static inline int nb_gpusel_scan(const char *root, struct nb_gpu_node *out,
                                 int max)
{
    char dir[256], p[512], link[512];
    DIR *d;
    struct dirent *e;
    int n = 0;

    snprintf(dir, sizeof(dir), "%s/sys/class/drm", root ? root : "");
    d = opendir(dir);
    if (!d) {
        return 0;
    }
    while ((e = readdir(d)) && n < max) {
        struct nb_gpu_node *g = &out[n];
        ssize_t k;
        const char *slash;

        if (strncmp(e->d_name, "renderD", 7) || strlen(e->d_name) > 16) {
            continue;
        }
        memset(g, 0, sizeof(*g));
        snprintf(g->path, sizeof(g->path), "/dev/dri/%s", e->d_name);
        snprintf(p, sizeof(p), "%s/%s/device/vendor", dir, e->d_name);
        g->vendor = (unsigned)nb_gpusel_read_num(p);
        snprintf(p, sizeof(p), "%s/%s/device/boot_vga", dir, e->d_name);
        g->boot_vga = nb_gpusel_read_num(p) == 1;
        snprintf(p, sizeof(p), "%s/%s/device/mem_info_vram_total", dir,
                 e->d_name);
        g->vram = nb_gpusel_read_num(p);
        snprintf(p, sizeof(p), "%s/%s/device", dir, e->d_name);
        k = readlink(p, link, sizeof(link) - 1);
        if (k > 0) {
            link[k] = '\0';
            slash = strrchr(link, '/');
            snprintf(g->slot, sizeof(g->slot), "%.15s", slash ? slash + 1 : link);
        }
        n++;
    }
    closedir(d);
    qsort(out, (size_t)n, sizeof(out[0]), nb_gpusel_cmp);
    return n;
}

/*
 * Resolve a --gpu value to a render node for the GPU pass.  auto: *path is
 * left empty (the per-frame order of nb_gpu_order applies).  Returns false
 * when dgpu/igpu names a GPU this machine does not have; why says what was
 * chosen or missing either way.
 */
static inline bool nb_gpusel_resolve(const char *want,
                                     const struct nb_gpu_node *g, int n,
                                     char *path, size_t pn, char *why,
                                     size_t wn)
{
    path[0] = '\0';
    if (!want || !*want || !strcmp(want, "auto")) {
        snprintf(why, wn, "auto: the GPU that imports the guest's frames, "
                 "the one that made them first");
        return true;
    }
    if (want[0] == '/') {
        snprintf(path, pn, "%s", want);
        snprintf(why, wn, "%s: set by --gpu / viewer.gpu", want);
        return true;
    }
    if (strcmp(want, "dgpu") && strcmp(want, "igpu")) {
        snprintf(why, wn, "unknown --gpu value \"%s\" (auto, dgpu, igpu or "
                 "a render node)", want);
        return false;
    }
    {
        bool d = !strcmp(want, "dgpu");
        int i = nb_gpu_pick(g, n, d);

        if (i < 0) {
            snprintf(why, wn, "%s: this machine has no %s GPU", want,
                     d ? "discrete" : "integrated");
            return false;
        }
        snprintf(path, pn, "%s", g[i].path);
        snprintf(why, wn, "%s: %s, %s %s at %s%s", want, g[i].path,
                 nb_gpu_vendor_name(g[i].vendor),
                 nb_gpu_kind_name(nb_gpu_classify(&g[i])), g[i].slot,
                 g[i].boot_vga ? ", boot VGA" : "");
        return true;
    }
}

#endif /* NB_GPUSEL_H */
