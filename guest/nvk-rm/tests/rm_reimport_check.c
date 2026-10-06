/* SPDX-License-Identifier: MIT */
/*
 * rm_reimport_check: the second process of a shared surface, on the host.
 * (Linux host only; the consumer half for rm_export_exec.)
 *
 * What the backend's RmResourceImport and NVK in the opening process do with
 * an RM-export resource (docs/VENUS.md "RM-export resources in a second
 * process", guest/windows/docs/shared-surfaces.md): the backend holds the
 * resource's dma-buf; it imports it on ANOTHER file of the render node
 * (PRIME_FD_TO_HANDLE); the opener exports that GEM object to a control
 * descriptor of its own (DRM_NVIDIA_GEM_EXPORT_NVKMS_MEMORY) and imports it
 * into its own, separate RM client (NV0000_CTRL_CMD_OS_UNIX_IMPORT_OBJECT_FROM_FD).
 * Then it CPU-maps that RM memory and checks the creator's coordinate pattern
 * (rm_export_exec's), and writes into it; the creator's mapping is the same
 * memory, which rm_export_exec does not look at again, so the check here is
 * the read.
 *
 * Build (from guest/rmclient, after `make`):
 *   cc -std=gnu11 -O2 -Wall -Iinclude -Isrc $(pkg-config --cflags libdrm) \
 *      ../nvk-rm/tests/rm_reimport_check.c build-make/librmclient.a \
 *      -ldrm -pthread -o build-make/rm_reimport_check
 * Run:
 *   build-make/rm_export_exec linear build-make/rm_reimport_check
 *   build-make/rm_export_exec bl5 build-make/rm_reimport_check
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

#include <xf86drm.h>

#include "rmclient.h"
#include "nv_ioctl_defs.h"

#define BPP 4u

struct drm_nvidia_gem_export_nvkms_memory_params {
    uint32_t handle;
    uint32_t pad;
    uint64_t nvkms_params_ptr;
    uint64_t nvkms_params_size;
};
struct nvkms_kapi_priv_export_memory_params {
    int32_t memFd;
};
#define DRM_IOCTL_NVIDIA_GEM_EXPORT_NVKMS_MEMORY \
    DRM_IOWR(DRM_COMMAND_BASE + 0x09, struct drm_nvidia_gem_export_nvkms_memory_params)

#define NV0000_CTRL_CMD_OS_UNIX_IMPORT_OBJECT_FROM_FD 0x3d06u
typedef struct {
    int32_t fd;
    struct {
        uint32_t type;
        struct {
            uint32_t hDevice, hParent, hObject;
        } rmObject;
    } object;
} import_from_fd_params;
_Static_assert(sizeof(import_from_fd_params) == 20, "IMPORT_OBJECT_FROM_FD");

static int failures;

static void report(bool ok, const char *what, int r)
{
    printf("[%s] reimport: %s (%d %s)\n", ok ? " ok " : "FAIL", what, r,
           r > 0 ? crm_status_name(r) : r < 0 ? strerror(-r) : "ok");
    fflush(stdout);
    if (!ok)
        failures++;
}

/* rm_export_exec's layouts (NIL TuringColor2D GOBs) */
static inline uint32_t gob_off(uint32_t xb, uint32_t y)
{
    return (xb / 32) * 256 + (y / 4) * 128 + ((xb % 32) / 16) * 64 + (y % 4) * 16 + (xb % 16);
}

static uint64_t px_off(uint64_t mod, uint32_t pitch, uint32_t x, uint32_t y)
{
    if (mod == 0)
        return (uint64_t)y * pitch + (uint64_t)x * BPP;
    const uint32_t h = (uint32_t)(mod & 0xf);
    const uint32_t xb = x * BPP;
    const uint32_t bh = 8u << h;
    const uint64_t block = (uint64_t)(y / bh) * (pitch / 64) + xb / 64;
    return block * (512u << h) + ((y % bh) / 8) * 512u + gob_off(xb % 64, y % 8);
}

int main(void)
{
    const char *e = getenv("RM_DMABUF_FD");
    if (!e) {
        fprintf(stderr, "run me under rm_export_exec\n");
        return 2;
    }
    const int dmabuf = atoi(e);
    const uint64_t size = strtoull(getenv("RM_SIZE"), NULL, 0);
    const uint64_t mod = strtoull(getenv("RM_MODIFIER"), NULL, 16);
    const uint32_t pitch = (uint32_t)atoi(getenv("RM_PITCH"));
    const uint32_t w = (uint32_t)atoi(getenv("RM_W")), h = (uint32_t)atoi(getenv("RM_H"));
    printf("       reimport: dma-buf %d, %" PRIu64 " B, modifier 0x%016" PRIx64 ", pitch %u\n",
           dmabuf, size, mod, pitch);

    /* The backend's half: the held dma-buf on another file of the node */
    int drm = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
    uint32_t gem = 0;
    int r = drm < 0 ? -errno : (drmPrimeFDToHandle(drm, dmabuf, &gem) ? -errno : 0);
    report(r == 0 && gem, "PRIME_FD_TO_HANDLE on a second file of the render node", r);
    if (r)
        return 1;
    /* The resource's dma-buf is all the backend needs to keep; the opener's
     * import below does not depend on it. */

    /* The opener's half: its own RM client, its own control descriptor */
    crm_client *c = NULL;
    r = crm_open(&c, NULL);
    report(r == 0, "a second RM client", r);
    if (r)
        return 1;
    uint32_t dev = 0;
    NV0080_ALLOC_PARAMETERS dp;
    memset(&dp, 0, sizeof dp);
    r = crm_alloc(c, crm_root(c), &dev, NV01_DEVICE_0, &dp, sizeof dp);
    report(r == 0, "its device", r);

    int ctl = open("/dev/nvidiactl", O_RDWR | O_CLOEXEC);
    struct nvkms_kapi_priv_export_memory_params nv = { .memFd = ctl };
    struct drm_nvidia_gem_export_nvkms_memory_params ex = {
        .handle = gem,
        .nvkms_params_ptr = (uint64_t)(uintptr_t)&nv,
        .nvkms_params_size = sizeof nv,
    };
    r = drmIoctl(drm, DRM_IOCTL_NVIDIA_GEM_EXPORT_NVKMS_MEMORY, &ex) ? -errno : 0;
    report(r == 0, "DRM_NVIDIA_GEM_EXPORT_NVKMS_MEMORY to our control descriptor", r);
    if (r)
        return 1;

    uint32_t mem = crm_new_handle(c);
    import_from_fd_params im = {
        .fd = ctl,
        .object = { .type = NV0000_CTRL_OS_UNIX_EXPORT_OBJECT_TYPE_RM,
                    .rmObject = { dev, dev, mem } },
    };
    r = crm_control(c, crm_root(c), NV0000_CTRL_CMD_OS_UNIX_IMPORT_OBJECT_FROM_FD, &im, sizeof im);
    report(r == 0, "OS_UNIX_IMPORT_OBJECT_FROM_FD into our client", r);
    close(ctl);
    if (r)
        return 1;
    /* The GEM handle was only the envelope */
    struct drm_gem_close gc = { .handle = gem };
    drmIoctl(drm, DRM_IOCTL_GEM_CLOSE, &gc);
    close(drm);

    void *p = NULL;
    r = crm_map_memory(c, dev, mem, 0, size, 0, &p);
    report(r == 0, "CPU-map the imported RM memory", r);
    if (r)
        return 1;
    uint64_t bad = 0;
    for (uint32_t y = 0; y < h; y++)
        for (uint32_t x = 0; x < w; x++)
            bad += *(volatile uint32_t *)((uint8_t *)p + px_off(mod, pitch, x, y)) !=
                   ((y << 16) | x);
    printf("       reimport: %" PRIu64 " of %u pixels differ\n", bad, w * h);
    report(bad == 0, "the creator's pattern, through our own RM client", 0);
    crm_unmap_memory(c, dev, mem, p, size, 0);
    crm_free(c, dev, mem);
    crm_release_handle(c, mem);
    crm_free(c, crm_root(c), dev);
    crm_close(c);
    printf("%s\n", failures ? "REIMPORT FAILED" : "REIMPORT PASSED");
    return failures ? 1 : 0;
}
