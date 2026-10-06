/* SPDX-License-Identifier: MIT */
/*
 * rm_export_exec: an RM-exported dma-buf for a host-side consumer to test
 * with.  (Linux host only.)
 *
 * The RM half of host_import_spike.c (spike/host-nvk-import): RM vidmem
 * allocated the way nvk-rm does (patch 0004), the coordinate pattern
 * (y << 16) | x written through the CPU mapping in the layout under test
 * (LINEAR, or NVIDIA block-linear 2D h = 5, 4 or 0 as NIL lays it out),
 * exported exactly as nvk-rm exports an image (patch 0013):
 * NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD, DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY
 * on the host's render node with NVK's NVKMS surface parameters, PRIME.
 * Then the GEM handle and the extra export are closed and CMD runs with the
 * dma-buf as its only reference, described in the environment:
 * RM_DMABUF_FD, RM_SIZE (dma-buf bytes), RM_IMAGE_SIZE, RM_MODIFIER (hex),
 * RM_PITCH, RM_W, RM_H.
 *
 * With host/venus's venus-rm-import example as CMD it checks the host half
 * of RM-export blobs (docs/VENUS.md): conduit-venus imports the dma-buf as a
 * resource and a Venus context reads it through VkImportMemoryResourceInfoMESA
 * as a DRM-modifier image, pixel-exact.
 *
 * Needs a guest/rmclient whose nv_ioctl_defs.h has the NV0000 OS_UNIX
 * export controls (feat/nvk-rm-windows-transport, spike/host-nvk-import).
 * Build (from guest/rmclient, after `make`):
 *   cc -std=gnu11 -O2 -Wall -Iinclude -Isrc $(pkg-config --cflags libdrm) \
 *      ../nvk-rm/tests/rm_export_exec.c build-make/librmclient.a \
 *      -ldrm -pthread -o build-make/rm_export_exec
 * Run (conduit-venus listening on SOCK):
 *   build-make/rm_export_exec bl5 \
 *      host/venus/target/release/examples/venus-rm-import SOCK
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <stdarg.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

#include <xf86drm.h>
#include <drm_fourcc.h>

#include "rmclient.h"
#include "nv_ioctl_defs.h"

#define W 1920u
#define H 1080u
#define BPP 4u

/* ---- nvidia-drm / NVKMS (nvidia-drm-ioctl.h, nvkms-kapi-private.h) ------ */

struct drm_nvidia_get_dev_info_params {
    uint32_t gpu_id, mig_device, primary_index, supports_alloc;
    uint32_t generic_page_kind, page_kind_generation, sector_layout;
    uint32_t supports_sync_fd, supports_semsurf;
};
struct drm_nvidia_gem_import_nvkms_memory_params {
    uint64_t mem_size;
    uint64_t nvkms_params_ptr;
    uint64_t nvkms_params_size;
    uint32_t handle;
    uint32_t pad;
};
struct nvkms_kapi_priv_import_memory_params {
    int32_t memFd;
    uint32_t layout; /* 0 block linear, 1 pitch */
    uint32_t log2_gobs_x, log2_gobs_y, log2_gobs_z;
    uint32_t pitch_in_blocks;
    uint8_t generic_memory;
    uint8_t pad[3];
};
_Static_assert(sizeof(struct drm_nvidia_get_dev_info_params) == 36, "GET_DEV_INFO");
_Static_assert(sizeof(struct drm_nvidia_gem_import_nvkms_memory_params) == 32, "GEM_IMPORT");
_Static_assert(sizeof(struct nvkms_kapi_priv_import_memory_params) == 28, "NVKMS import");

#define DRM_IOCTL_NVIDIA_GET_DEV_INFO \
    DRM_IOWR(DRM_COMMAND_BASE + 0x03, struct drm_nvidia_get_dev_info_params)
#define DRM_IOCTL_NVIDIA_GEM_IMPORT_NVKMS_MEMORY \
    DRM_IOWR(DRM_COMMAND_BASE + 0x01, struct drm_nvidia_gem_import_nvkms_memory_params)

#define NVOS32_ATTR_PHYSICALITY_ALLOW_NONCONTIGUOUS 0x3u
#define NVOS32_ATTR2_ZBC_PREFER_NO_ZBC 0x2u

/* ---- helpers -------------------------------------------------------------- */

static int failures;

static void report(bool ok, const char *fmt, ...) __attribute__((format(printf, 2, 3)));
static void report(bool ok, const char *fmt, ...)
{
    va_list ap;
    va_start(ap, fmt);
    printf("[%s] ", ok ? " ok " : "FAIL");
    vprintf(fmt, ap);
    printf("\n");
    va_end(ap);
    fflush(stdout);
    if (!ok)
        failures++;
}

/* Coordinate-encoding pattern: a pixel's value names the pixel. */
static inline uint32_t pat(uint32_t x, uint32_t y) { return (y << 16) | x; }
/* Write-check pattern: distinguishable from pat() by the top byte. */

/* ---- layouts -------------------------------------------------------------- */

struct layout {
    const char *name;
    uint64_t modifier;
    bool block_linear;
    uint32_t log2_gob_h; /* block height in GOBs, log2 */
    uint32_t pitch;      /* bytes per row of pixels (linear) / of GOBs*64 (BL) */
    uint64_t size;       /* bytes the layout occupies */
};

/* NIL's TuringColor2D GOB (64 B x 8 rows, 16 B x 2 row sectors; see
 * src/nouveau/nil/copy.rs CopyGOBTuring2D). Byte x (0..63), row y (0..7). */
static inline uint32_t gob_off(uint32_t xb, uint32_t y)
{
    return (xb / 32) * 256 + (y / 4) * 128 + ((xb % 32) / 16) * 64 + (y % 4) * 16 + (xb % 16);
}

/* Offset of pixel (x, y) in the layout. Block-linear 2D: blocks are 1 GOB
 * wide and 2^h GOBs tall, laid out row-major; GOBs inside a block stacked
 * vertically. */
static inline uint64_t px_off(const struct layout *l, uint32_t x, uint32_t y)
{
    if (!l->block_linear)
        return (uint64_t)y * l->pitch + (uint64_t)x * BPP;
    const uint32_t xb = x * BPP;
    const uint32_t gobs_x = l->pitch / 64;
    const uint32_t bh = 8u << l->log2_gob_h; /* rows per block */
    const uint64_t block = (uint64_t)(y / bh) * gobs_x + xb / 64;
    const uint32_t gob_in_block = (y % bh) / 8;
    return block * (512u << l->log2_gob_h) + gob_in_block * 512u + gob_off(xb % 64, y % 8);
}

static void layout_init(struct layout *l, const char *name, uint64_t mod)
{
    memset(l, 0, sizeof *l);
    l->name = name;
    l->modifier = mod;
    if (mod == DRM_FORMAT_MOD_LINEAR) {
        l->pitch = W * BPP; /* 7680, 256-aligned */
        l->size = (uint64_t)l->pitch * H;
    } else {
        l->block_linear = true;
        l->log2_gob_h = (uint32_t)(mod & 0xf);
        l->pitch = (W * BPP + 63) & ~63u;
        const uint32_t bh = 8u << l->log2_gob_h;
        const uint32_t brows = (H + bh - 1) / bh;
        l->size = (uint64_t)(l->pitch / 64) * brows * (512u << l->log2_gob_h);
    }
}

/* ---- RM side -------------------------------------------------------------- */

struct rm {
    crm_client *c;
    uint32_t dev, sub;
    int drm_fd;
};

struct surf {
    uint32_t mem;
    uint64_t size;
    volatile uint8_t *cpu;
    uint32_t gem;
    int dmabuf_fd;
    int rm_export_fd; /* a second RM export (for the opaque-fd probe) */
};

static int rm_alloc(struct rm *rm, uint64_t want, struct surf *s)
{
    memset(s, 0, sizeof *s);
    s->dmabuf_fd = s->rm_export_fd = -1;
    NV_MEMORY_ALLOCATION_PARAMS mp;
    memset(&mp, 0, sizeof mp);
    mp.owner = crm_root(rm->c);
    mp.type = NVOS32_TYPE_IMAGE;
    mp.flags = NVOS32_ALLOC_FLAGS_ALIGNMENT_FORCE;
    /* nvk-rm's VRAM allocation (patch 0004): no width/height/format/kind. */
    mp.attr = (NVOS32_ATTR_LOCATION_VIDMEM << NVOS32_ATTR_LOCATION_SHIFT) |
              (NVOS32_ATTR_PAGE_SIZE_BIG << NVOS32_ATTR_PAGE_SIZE_SHIFT) |
              (NVOS32_ATTR_PHYSICALITY_ALLOW_NONCONTIGUOUS << NVOS32_ATTR_PHYSICALITY_SHIFT);
    mp.attr2 = NVOS32_ATTR2_ZBC_PREFER_NO_ZBC |
               (NVOS32_ATTR2_GPU_CACHEABLE_YES << NVOS32_ATTR2_GPU_CACHEABLE_SHIFT);
    mp.size = (want + 0xffff) & ~0xffffull;
    mp.alignment = 64 * 1024;
    int r = crm_alloc(rm->c, rm->dev, &s->mem, NV01_MEMORY_LOCAL_USER, &mp, sizeof mp);
    report(r == 0, "  RM alloc NV01_MEMORY_LOCAL_USER 0x%" PRIx64 " B -> 0x%08x (%s)",
           (uint64_t)((want + 0xffff) & ~0xffffull), s->mem, crm_status_name(r));
    if (r)
        return r;
    s->size = mp.size;
    void *p = NULL;
    r = crm_map_memory(rm->c, rm->dev, s->mem, 0, s->size, 0, &p);
    report(r == 0, "  RM CPU map (%s)", crm_status_name(r));
    if (r)
        return r;
    s->cpu = p;
    return 0;
}

static int rm_export_fd(struct rm *rm, struct surf *s, int *out)
{
    int ctl = open("/dev/nvidiactl", O_RDWR | O_CLOEXEC);
    if (ctl < 0)
        return -errno;
    NV0000_CTRL_OS_UNIX_EXPORT_OBJECT_TO_FD_PARAMS ex;
    memset(&ex, 0, sizeof ex);
    ex.object.type = NV0000_CTRL_OS_UNIX_EXPORT_OBJECT_TYPE_RM;
    ex.object.data.rmObject.hDevice = rm->dev;
    ex.object.data.rmObject.hParent = rm->dev;
    ex.object.data.rmObject.hObject = s->mem;
    ex.fd = ctl;
    int r = crm_control(rm->c, crm_root(rm->c), NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD, &ex,
                        sizeof ex);
    if (r) {
        close(ctl);
        return r;
    }
    *out = ctl;
    return 0;
}

static int rm_export(struct rm *rm, const struct layout *l, struct surf *s)
{
    int ctl = -1;
    int r = rm_export_fd(rm, s, &ctl);
    report(r == 0, "  NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD (%s)", crm_status_name(r));
    if (r)
        return r;

    /* nvk-rm's surface params (patch 0013 export_to_gem). */
    struct nvkms_kapi_priv_import_memory_params nv;
    memset(&nv, 0, sizeof nv);
    nv.memFd = ctl;
    nv.layout = l->block_linear ? 0 : 1;
    nv.log2_gobs_y = l->block_linear ? l->log2_gob_h : 0;
    nv.generic_memory = l->block_linear;
    struct drm_nvidia_gem_import_nvkms_memory_params imp;
    memset(&imp, 0, sizeof imp);
    imp.mem_size = s->size;
    imp.nvkms_params_ptr = (uint64_t)(uintptr_t)&nv;
    imp.nvkms_params_size = sizeof nv;
    r = drmIoctl(rm->drm_fd, DRM_IOCTL_NVIDIA_GEM_IMPORT_NVKMS_MEMORY, &imp) ? -errno : 0;
    close(ctl);
    report(r == 0 && imp.handle, "  DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY -> GEM %u (%s)", imp.handle,
           r ? strerror(-r) : "ok");
    if (r)
        return r;
    s->gem = imp.handle;

    r = drmPrimeHandleToFD(rm->drm_fd, s->gem, DRM_CLOEXEC | DRM_RDWR, &s->dmabuf_fd);
    if (r)
        r = -errno;
    off_t dsz = r ? -1 : lseek(s->dmabuf_fd, 0, SEEK_END);
    report(r == 0, "  PRIME handle -> dma-buf fd %d (size 0x%llx)", s->dmabuf_fd,
           (unsigned long long)dsz);
    if (r)
        return r;

    r = rm_export_fd(rm, s, &s->rm_export_fd);
    if (r)
        s->rm_export_fd = -1;
    return 0;
}

static void rm_free(struct rm *rm, struct surf *s)
{
    if (s->dmabuf_fd >= 0)
        close(s->dmabuf_fd);
    if (s->rm_export_fd >= 0)
        close(s->rm_export_fd);
    if (s->gem) {
        struct drm_gem_close gc = { .handle = s->gem };
        drmIoctl(rm->drm_fd, DRM_IOCTL_GEM_CLOSE, &gc);
    }
    if (s->cpu)
        crm_unmap_memory(rm->c, rm->dev, s->mem, (void *)s->cpu, s->size, 0);
    if (s->mem)
        crm_free(rm->c, rm->dev, s->mem);
    memset(s, 0, sizeof *s);
}

static void fill(const struct layout *l, struct surf *s)
{
    /* Background in the unused tail/padding: a marker. */
    for (uint64_t o = 0; o < s->size; o += 4)
        *(volatile uint32_t *)(s->cpu + o) = 0xdeadbeefu;
    for (uint32_t y = 0; y < H; y++)
        for (uint32_t x = 0; x < W; x++)
            *(volatile uint32_t *)(s->cpu + px_off(l, x, y)) = pat(x, y);
}


/* ---- rm_export_exec ------------------------------------------------------
 * Allocate RM vidmem the way nvk-rm does, write the coordinate pattern in
 * the layout under test, export it through nvidia-drm as a dma-buf (exactly
 * as above), then run argv[2..] with the dma-buf inherited and described in
 * the environment: RM_DMABUF_FD, RM_SIZE (dma-buf bytes), RM_MODIFIER (hex),
 * RM_PITCH, RM_IMAGE_SIZE, RM_W, RM_H. The RM client stays open (this
 * process waits) so nothing but the dma-buf decides the memory's life.
 *
 *   rm_export_exec linear|bl5|bl4|bl0 CMD [ARGS...]
 */
#include <sys/wait.h>

int main(int argc, char **argv)
{
    if (argc < 3) {
        fprintf(stderr, "usage: %s linear|bl5|bl4|bl0 CMD [ARGS...]\n", argv[0]);
        return 2;
    }
    uint64_t mod = DRM_FORMAT_MOD_LINEAR;
    if (!strcmp(argv[1], "bl5"))
        mod = DRM_FORMAT_MOD_NVIDIA_BLOCK_LINEAR_2D(0, 1, 2, 0x06, 5);
    else if (!strcmp(argv[1], "bl4"))
        mod = DRM_FORMAT_MOD_NVIDIA_BLOCK_LINEAR_2D(0, 1, 2, 0x06, 4);
    else if (!strcmp(argv[1], "bl0"))
        mod = DRM_FORMAT_MOD_NVIDIA_BLOCK_LINEAR_2D(0, 1, 2, 0x06, 0);
    else if (strcmp(argv[1], "linear"))
        return 2;
    struct layout l;
    layout_init(&l, argv[1], mod);

    struct rm rm = { .drm_fd = -1 };
    int r = crm_open(&rm.c, NULL);
    report(r == 0, "crm_open (%s)", crm_status_name(r));
    if (r)
        return 1;
    NV0080_ALLOC_PARAMETERS dp;
    memset(&dp, 0, sizeof dp);
    r = crm_alloc(rm.c, crm_root(rm.c), &rm.dev, NV01_DEVICE_0, &dp, sizeof dp);
    NV2080_ALLOC_PARAMETERS sp = { .subDeviceId = 0 };
    if (!r)
        r = crm_alloc(rm.c, rm.dev, &rm.sub, NV20_SUBDEVICE_0, &sp, sizeof sp);
    report(r == 0, "RM device + subdevice (%s)", crm_status_name(r));
    if (r)
        return 1;
    rm.drm_fd = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
    if (rm.drm_fd < 0)
        return 1;

    struct surf s;
    if (rm_alloc(&rm, l.size, &s))
        return 1;
    fill(&l, &s);
    if (rm_export(&rm, &l, &s))
        return 1;
    off_t dsz = lseek(s.dmabuf_fd, 0, SEEK_END);
    lseek(s.dmabuf_fd, 0, SEEK_SET);
    /* Close everything but the dma-buf before the consumer runs: the GEM
     * handle and the second RM export. */
    struct drm_gem_close gc = { .handle = s.gem };
    drmIoctl(rm.drm_fd, DRM_IOCTL_GEM_CLOSE, &gc);
    s.gem = 0;
    if (s.rm_export_fd >= 0) {
        close(s.rm_export_fd);
        s.rm_export_fd = -1;
    }

    char buf[64];
    snprintf(buf, sizeof buf, "%d", s.dmabuf_fd);
    setenv("RM_DMABUF_FD", buf, 1);
    snprintf(buf, sizeof buf, "%lld", (long long)dsz);
    setenv("RM_SIZE", buf, 1);
    snprintf(buf, sizeof buf, "0x%016" PRIx64, l.modifier);
    setenv("RM_MODIFIER", buf, 1);
    snprintf(buf, sizeof buf, "%u", l.pitch);
    setenv("RM_PITCH", buf, 1);
    snprintf(buf, sizeof buf, "%" PRIu64, l.size);
    setenv("RM_IMAGE_SIZE", buf, 1);
    snprintf(buf, sizeof buf, "%u", W);
    setenv("RM_W", buf, 1);
    snprintf(buf, sizeof buf, "%u", H);
    setenv("RM_H", buf, 1);
    fflush(stdout);

    pid_t pid = fork();
    if (pid == 0) {
        /* The dma-buf survives exec. */
        int fl = fcntl(s.dmabuf_fd, F_GETFD);
        fcntl(s.dmabuf_fd, F_SETFD, fl & ~FD_CLOEXEC);
        execvp(argv[2], argv + 2);
        perror("execvp");
        _exit(127);
    }
    /* Ours goes now: only the consumer's copy (and what it hands on) holds
     * the memory. */
    close(s.dmabuf_fd);
    s.dmabuf_fd = -1;
    int st = 0;
    waitpid(pid, &st, 0);
    rm_free(&rm, &s);
    close(rm.drm_fd);
    crm_free(rm.c, rm.dev, rm.sub);
    crm_free(rm.c, crm_root(rm.c), rm.dev);
    crm_close(rm.c);
    return WIFEXITED(st) ? WEXITSTATUS(st) : 1;
}
