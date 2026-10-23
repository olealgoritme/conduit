/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * nb_vkpresent.h -- the picture through a Vulkan swapchain, for NVIDIA Smooth
 * Motion (--framegen=nvidia, docs/VIEWER-ENHANCE.md).
 *
 * Smooth Motion is the NVIDIA Linux driver's own frame generation: an
 * implicit Vulkan layer (VK_LAYER_NV_present, libnvidia-present.so) that the
 * loader enables for a process with NVPRESENT_ENABLE_SMOOTH_MOTION=1, on RTX
 * 40 and 50 GPUs.  It acts on vkQueuePresentKHR only.  The viewer normally
 * hands the guest's dma-bufs to the compositor as they are (or through its
 * GLES pass), so the layer never sees a present.  In this mode each guest
 * frame is imported into Vulkan, blitted (scaled, letterboxed) into a
 * swapchain image of the window's size, and presented -- one present per
 * guest frame -- and the driver generates the frames in between.
 *
 * Never blocking on the display: an image is acquired with a zero timeout;
 * none free means "not now" and the caller keeps the frame staged.  The blit
 * is waited for (bounded) so the guest's buffer can go back at once.
 *
 * libvulkan is dlopen'ed, as EGL is for nb_gpuscale.c.  Without the Vulkan
 * headers at build time the presenter is left out and reports itself as
 * unavailable (`make` says so).
 *
 * The availability checks and the present-mode choice are pure:
 * test/test_vkpresent.c drives them.
 */
#ifndef NB_VKPRESENT_H
#define NB_VKPRESENT_H

#include <dirent.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* ── availability (pure, testable) ─────────────────────────────────────── */

#define NB_VKP_NVIDIA     0x10deu
/* The first Ada (RTX 40) PCI device id; every later NVIDIA GPU (Ada,
 * Blackwell: RTX 50) has a higher one.  Smooth Motion needs Ada or later. */
#define NB_VKP_ADA_FIRST  0x2680u
#define NB_VKP_LAYER      "VK_LAYER_NV_present"
#define NB_VKP_ENV        "NVPRESENT_ENABLE_SMOOTH_MOTION"

static inline bool nb_vkp_gpu_ok(unsigned vendor, unsigned device)
{
    return vendor == NB_VKP_NVIDIA && device >= NB_VKP_ADA_FIRST;
}

/* Whether a layer manifest in `dir` (*.json) names VK_LAYER_NV_present. */
static inline bool nb_vkp_layer_in_dir(const char *dir)
{
    DIR *d = dir ? opendir(dir) : NULL;
    struct dirent *e;
    bool found = false;

    while (d && !found && (e = readdir(d))) {
        size_t n = strlen(e->d_name);
        char path[512], buf[8192];
        FILE *f;
        size_t k;

        if (n < 6 || strcmp(e->d_name + n - 5, ".json")) {
            continue;
        }
        snprintf(path, sizeof(path), "%s/%s", dir, e->d_name);
        f = fopen(path, "re");
        if (!f) {
            continue;
        }
        k = fread(buf, 1, sizeof(buf) - 1, f);
        fclose(f);
        buf[k] = '\0';
        found = strstr(buf, "\"" NB_VKP_LAYER "\"") != NULL;
    }
    if (d) {
        closedir(d);
    }
    return found;
}

/* The loader's implicit-layer directories (the system ones and the XDG
 * ones), searched for the layer.  `root` prefixes every path (tests). */
static inline bool nb_vkp_layer_present(const char *root)
{
    static const char *const fixed[] = {
        "/etc/vulkan/implicit_layer.d",
        "/usr/local/share/vulkan/implicit_layer.d",
        "/usr/share/vulkan/implicit_layer.d",
        "/etc/xdg/vulkan/implicit_layer.d",
    };
    const char *dirs = getenv("XDG_DATA_DIRS");
    const char *home = getenv("XDG_DATA_HOME");
    char path[512];

    root = root ? root : "";
    for (size_t i = 0; i < sizeof(fixed) / sizeof(fixed[0]); i++) {
        snprintf(path, sizeof(path), "%s%s", root, fixed[i]);
        if (nb_vkp_layer_in_dir(path)) {
            return true;
        }
    }
    if (home && *home) {
        snprintf(path, sizeof(path), "%s%s/vulkan/implicit_layer.d", root,
                 home);
        if (nb_vkp_layer_in_dir(path)) {
            return true;
        }
    }
    while (dirs && *dirs) {
        const char *c = strchr(dirs, ':');
        int len = c ? (int)(c - dirs) : (int)strlen(dirs);

        snprintf(path, sizeof(path), "%s%.*s/vulkan/implicit_layer.d", root,
                 len, dirs);
        if (len > 0 && nb_vkp_layer_in_dir(path)) {
            return true;
        }
        dirs = c ? c + 1 : NULL;
    }
    return false;
}

/* An Ada-or-later NVIDIA GPU among the DRM render nodes under `root`
 * (sysfs: class/drm/renderD* /device/{vendor,device}). */
static inline bool nb_vkp_gpu_present(const char *root)
{
    char base[256], path[sizeof(base) + 256 + 32];
    DIR *d;
    struct dirent *e;
    bool ok = false;

    snprintf(base, sizeof(base), "%s/sys/class/drm", root ? root : "");
    d = opendir(base);
    while (d && !ok && (e = readdir(d))) {
        unsigned v = 0, dev = 0;
        FILE *f;

        if (strncmp(e->d_name, "renderD", 7)) {
            continue;
        }
        snprintf(path, sizeof(path), "%s/%s/device/vendor", base, e->d_name);
        if ((f = fopen(path, "re"))) {
            if (fscanf(f, "%x", &v) != 1) {
                v = 0;
            }
            fclose(f);
        }
        snprintf(path, sizeof(path), "%s/%s/device/device", base, e->d_name);
        if ((f = fopen(path, "re"))) {
            if (fscanf(f, "%x", &dev) != 1) {
                dev = 0;
            }
            fclose(f);
        }
        ok = nb_vkp_gpu_ok(v, dev);
    }
    if (d) {
        closedir(d);
    }
    return ok;
}

/* Present modes, in Vulkan's numbering (VkPresentModeKHR). */
#define NB_VKP_IMMEDIATE 0
#define NB_VKP_MAILBOX   1
#define NB_VKP_FIFO      2

/*
 * The present mode for a wanted one among those the surface offers: the
 * viewer's presentation setting maps to IMMEDIATE (immediate now), MAILBOX
 * (latest per refresh) or FIFO (vsync).  IMMEDIATE falls back to MAILBOX,
 * MAILBOX to FIFO; FIFO is always there.
 */
static inline int nb_vkp_pick_mode(const int *have, unsigned n, int want)
{
    bool im = false, mb = false;

    for (unsigned i = 0; i < n; i++) {
        im |= have[i] == NB_VKP_IMMEDIATE;
        mb |= have[i] == NB_VKP_MAILBOX;
    }
    if (want == NB_VKP_IMMEDIATE && im) {
        return NB_VKP_IMMEDIATE;
    }
    if (want != NB_VKP_FIFO && mb) {
        return NB_VKP_MAILBOX;
    }
    return NB_VKP_FIFO;
}

/* ── the presenter ─────────────────────────────────────────────────────── */

/*
 * Whether this host has Smooth Motion: a viewer built with Vulkan, an
 * Ada-or-later NVIDIA GPU and the driver's present layer.  Probed once; why
 * says what is missing.
 */
bool nb_vkp_available(const char **why);
/* NVPRESENT_ENABLE_SMOOTH_MOTION=1 for this process: before the first
 * Vulkan instance, which is when the loader reads it. */
void nb_vkp_env(void);

struct nb_vkp;

/* On a Wayland surface (a subsurface the presenter owns from now on) or an
 * X11 window.  NULL with why. */
struct nb_vkp *nb_vkp_open_wayland(void *wl_display, void *wl_surface,
                                   char *why, size_t n);
struct nb_vkp *nb_vkp_open_xcb(void *xcb_connection, uint32_t window,
                               char *why, size_t n);
void nb_vkp_close(struct nb_vkp *v);
const char *nb_vkp_device(const struct nb_vkp *v);

struct nb_vkp_frame {
    int      key;               /* the guest buffer's slot: import cache   */
    int      fd;                /* its dma-buf (not owned)                 */
    uint32_t w, h, stride, offset, fourcc;
    uint64_t modifier;
    double   sx, sy, sw, sh;    /* source rectangle                        */
    int      dx, dy, dw, dh;    /* destination in the swapchain image      */
    uint32_t out_w, out_h;      /* the swapchain: window size, pixels      */
    int      mode;              /* NB_VKP_IMMEDIATE / _MAILBOX / _FIFO     */
    bool     nearest;
};

/* 1 presented (the guest buffer has been read), 0 no free image now (try
 * again shortly), -errno failed (the caller leaves this mode). */
int  nb_vkp_present(struct nb_vkp *v, const struct nb_vkp_frame *f,
                    unsigned timeout_us, unsigned *gpu_us);
/* The guest buffer in `key` is gone: drop its import. */
void nb_vkp_forget(struct nb_vkp *v, int key);

#endif /* NB_VKPRESENT_H */
