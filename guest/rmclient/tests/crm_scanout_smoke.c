/* SPDX-License-Identifier: MIT */
/*
 * crm_scanout_smoke: zero-copy present of RM video memory on a Windows guest's
 * Conduit display, with nothing but librmclient's Windows transport.
 *
 *   crm_open -> device, subdevice -> NV01_MEMORY_LOCAL_USER, pitch linear,
 *   1920x1080 XRGB8888 (three buffers) -> CPU map, test pattern ->
 *   NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD into a fresh control channel ->
 *   DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY on a host DRM render node -> ScanoutFlip
 *   of that GEM object, re-flipped with a moving square for `seconds` ->
 *   GEM close, free, close.
 *
 * The same route NVK on RM takes on Linux (guest/nvk-rm patch 0013), except that
 * here the program sends the DRM ioctl and the ScanoutFlip itself through the
 * KMD's FORWARD; on Linux the guest kernel does both. Nothing is copied: the
 * host exports the GEM object as a dma-buf and the viewer shows that VRAM.
 *
 * Usage: crm_scanout_smoke [seconds=60] [dri_index=0] [frame_ms=50]
 */
#include <errno.h>
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "rmclient.h"
#include "rmclient_transport.h"
#include "nv_ioctl_defs.h"

#if defined(_WIN32)
#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#include <windows.h>

#define W 1920u
#define H 1080u
#define STRIDE (W * 4u)
#define NBUF 3
#define SQ 160u

/* nvidia-drm (kernel-open/nvidia-drm/nvidia-drm-ioctl.h) and the NVKMS block
 * its GEM import takes (nvkms-kapi-private.h); the backend matches the import
 * as 'd' nr 0x41 with the nested block at offset 32 and memFd first in it. */
struct drm_nvidia_gem_import_nvkms_memory_params {
    uint64_t mem_size;
    uint64_t nvkms_params_ptr;
    uint64_t nvkms_params_size;
    uint32_t handle;
    uint32_t pad;
};
struct nvkms_kapi_priv_import_memory_params {
    int32_t memFd;
    uint32_t layout; /* NvKmsSurfaceMemoryLayout: 0 block linear, 1 pitch */
    uint32_t log2_gobs_x, log2_gobs_y, log2_gobs_z;
    uint32_t pitch_in_blocks;
    uint8_t generic_memory;
    uint8_t pad[3];
};
_Static_assert(sizeof(struct drm_nvidia_gem_import_nvkms_memory_params) == 32, "GEM_IMPORT");
_Static_assert(sizeof(struct nvkms_kapi_priv_import_memory_params) == 28, "NVKMS import");

#define DRM_IOCTL_NVIDIA_GEM_IMPORT_NVKMS_MEMORY 0xC0206441u /* DRM_IOWR(0x40 + 0x01, 32) */
#define DRM_IOCTL_GEM_CLOSE 0x40086409u                      /* DRM_IOW(0x09, 8) */
#define DRM_FORMAT_XRGB8888 0x34325258u
#define DRM_FORMAT_MOD_LINEAR 0ull

#define NVOS32_ATTR_PHYSICALITY_ALLOW_NONCONTIGUOUS 0x3u
#define NVOS32_ATTR2_ZBC_PREFER_NO_ZBC 0x2u /* 1:0 */

static int failed;

static int step(const char *what, int r)
{
    if (r == 0)
        printf("[ ok ] %s\n", what);
    else
        printf("[FAIL] %s: %s (%d / 0x%x) %s\n", what, crm_status_name(r), r, (unsigned)r,
               crm_status_string(r));
    fflush(stdout);
    if (r)
        failed = 1;
    return r;
}

/* The background: eight bars over the top two thirds, a grey ramp under them,
 * a one-pixel red border (shows a wrong stride or offset at a glance). */
static uint32_t bg(uint32_t x, uint32_t y)
{
    static const uint32_t bars[8] = { 0xffffff, 0xffff00, 0x00ffff, 0x00ff00,
                                      0xff00ff, 0xff0000, 0x0000ff, 0x000000 };
    if (x == 0 || y == 0 || x == W - 1 || y == H - 1)
        return 0xff0000;
    if (y < H * 2 / 3)
        return bars[x * 8 / W];
    const uint32_t g = x * 255 / (W - 1);
    return (g << 16) | (g << 8) | g;
}

static void fill(volatile uint32_t *px)
{
    for (uint32_t y = 0; y < H; y++)
        for (uint32_t x = 0; x < W; x++)
            px[y * (STRIDE / 4) + x] = bg(x, y);
}

static void rect(volatile uint32_t *px, uint32_t x0, uint32_t y0, int square, uint32_t frame)
{
    for (uint32_t y = y0; y < y0 + SQ; y++)
        for (uint32_t x = x0; x < x0 + SQ; x++) {
            uint32_t v = bg(x, y);
            if (square) {
                /* orange, with a dark diagonal that turns with the frame number */
                const uint32_t d = (x - x0 + y - y0 + frame * 4) % SQ;
                v = d < 12 ? 0x202020 : 0xff8000;
            }
            px[y * (STRIDE / 4) + x] = v;
        }
}

/* Square position at frame k: bounces around the screen. */
static void square_at(uint32_t k, uint32_t *x, uint32_t *y)
{
    const uint32_t mx = W - SQ - 2, my = H - SQ - 2;
    uint32_t px = (k * 12) % (2 * mx), py = (k * 7) % (2 * my);
    *x = 1 + (px < mx ? px : 2 * mx - px);
    *y = 1 + (py < my ? py : 2 * my - py);
}

struct buf {
    uint32_t mem;
    uint64_t size;
    volatile uint32_t *cpu;
    uint32_t gem;
    int has_sq;
    uint32_t sq_x, sq_y;
};

/* RM memory -> GEM handle on the DRM node, as nvk-rm's export_to_gem. */
static int export_to_gem(crm_client *c, uint32_t dev, int drm_fd, struct buf *b)
{
    int ctl = -1;
    int r = crm_win_open_device(CRM_WIN_DEV_CTL, &ctl);
    char w[160];
    snprintf(w, sizeof w, "  open a fresh control channel for the export (handle %d)", ctl);
    if (step(w, r))
        return r;

    NV0000_CTRL_OS_UNIX_EXPORT_OBJECT_TO_FD_PARAMS ex;
    memset(&ex, 0, sizeof ex);
    ex.object.type = NV0000_CTRL_OS_UNIX_EXPORT_OBJECT_TYPE_RM;
    ex.object.data.rmObject.hDevice = dev;
    ex.object.data.rmObject.hParent = dev;
    ex.object.data.rmObject.hObject = b->mem;
    ex.fd = ctl;
    r = crm_control(c, crm_root(c), NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD, &ex, sizeof ex);
    if (step("  NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD", r))
        goto out;

    struct nvkms_kapi_priv_import_memory_params nv;
    memset(&nv, 0, sizeof nv);
    nv.memFd = ctl;
    nv.layout = 1; /* pitch */
    struct drm_nvidia_gem_import_nvkms_memory_params imp;
    memset(&imp, 0, sizeof imp);
    imp.mem_size = b->size;
    imp.nvkms_params_ptr = (uint64_t)(uintptr_t)&nv;
    imp.nvkms_params_size = sizeof nv;
    r = crm_win_ioctl(drm_fd, DRM_IOCTL_NVIDIA_GEM_IMPORT_NVKMS_MEMORY, &imp, sizeof imp, &nv,
                      sizeof nv);
    if (r == 0 && imp.handle == 0)
        r = -EIO;
    snprintf(w, sizeof w, "  DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY -> GEM handle %u", imp.handle);
    if (step(w, r) == 0)
        b->gem = imp.handle;
out:
    /* nvidia-drm holds its own reference now; the channel was the envelope. */
    crm_win_close_device(ctl);
    return r;
}

int main(int argc, char **argv)
{
    const unsigned seconds = argc > 1 ? (unsigned)strtoul(argv[1], NULL, 0) : 60;
    const unsigned dri = argc > 2 ? (unsigned)strtoul(argv[2], NULL, 0) : 0;
    const unsigned frame_ms = argc > 3 ? (unsigned)strtoul(argv[3], NULL, 0) : 50;

    crm_client *c = NULL;
    if (step("crm_open", crm_open(&c, NULL)))
        return 1;
    printf("       RM version %s, root client 0x%08x\n", crm_rm_version(c), crm_root(c));

    uint32_t dev = 0, sub = 0;
    int drm_fd = -1;
    struct buf bufs[NBUF];
    memset(bufs, 0, sizeof bufs);
    char w[200];

    NV0080_ALLOC_PARAMETERS dp;
    memset(&dp, 0, sizeof dp);
    if (step("alloc NV01_DEVICE_0", crm_alloc(c, crm_root(c), &dev, NV01_DEVICE_0, &dp, sizeof dp)))
        goto out;
    NV2080_ALLOC_PARAMETERS sp = { .subDeviceId = 0 };
    if (step("alloc NV20_SUBDEVICE_0", crm_alloc(c, dev, &sub, NV20_SUBDEVICE_0, &sp, sizeof sp)))
        goto out;

    int r = crm_win_open_device(CRM_WIN_DEV_DRI_BASE + dri, &drm_fd);
    snprintf(w, sizeof w, "open DRM render node %u (Open device_type %u) -> handle %d", dri,
             CRM_WIN_DEV_DRI_BASE + dri, drm_fd);
    if (step(w, r))
        goto out;

    const uint64_t want = ((uint64_t)STRIDE * H + 0xffff) & ~0xffffull;
    for (int i = 0; i < NBUF; i++) {
        struct buf *b = &bufs[i];
        NV_MEMORY_ALLOCATION_PARAMS mp;
        memset(&mp, 0, sizeof mp);
        mp.owner = crm_root(c);
        mp.type = NVOS32_TYPE_IMAGE;
        mp.flags = NVOS32_ALLOC_FLAGS_ALIGNMENT_FORCE;
        mp.width = W;
        mp.height = H;
        mp.pitch = (int32_t)STRIDE;
        /* What nvk-rm allocates VRAM with: 64 KiB pages, not necessarily contiguous. */
        mp.attr = (NVOS32_ATTR_LOCATION_VIDMEM << NVOS32_ATTR_LOCATION_SHIFT) |
                  (NVOS32_ATTR_PAGE_SIZE_BIG << NVOS32_ATTR_PAGE_SIZE_SHIFT) |
                  (NVOS32_ATTR_PHYSICALITY_ALLOW_NONCONTIGUOUS << NVOS32_ATTR_PHYSICALITY_SHIFT);
        mp.attr2 = NVOS32_ATTR2_ZBC_PREFER_NO_ZBC |
                   (NVOS32_ATTR2_GPU_CACHEABLE_YES << NVOS32_ATTR2_GPU_CACHEABLE_SHIFT);
        mp.size = want;
        mp.alignment = 64 * 1024;
        snprintf(w, sizeof w, "buffer %d: alloc NV01_MEMORY_LOCAL_USER %ux%u pitch %u (%" PRIu64
                 " bytes)", i, W, H, STRIDE, want);
        if (step(w, crm_alloc(c, dev, &b->mem, NV01_MEMORY_LOCAL_USER, &mp, sizeof mp)))
            goto cleanup;
        b->size = mp.size;
        printf("       memory 0x%08x, size 0x%" PRIx64 ", offset 0x%" PRIx64 ", attr 0x%08x\n",
               b->mem, mp.size, mp.offset, mp.attr);

        void *p = NULL;
        if (step("  CPU-map it", crm_map_memory(c, dev, b->mem, 0, b->size, 0, &p)))
            goto cleanup;
        b->cpu = p;
        const ULONGLONG t0 = GetTickCount64();
        fill(b->cpu);
        /* Read back a sample across the whole buffer, the last page included. */
        int ok = 1;
        for (uint32_t y = 0; y < H && ok; y += 37)
            for (uint32_t x = 0; x < W && ok; x += 101)
                ok = b->cpu[y * (STRIDE / 4) + x] == bg(x, y);
        ok = ok && b->cpu[(H - 1) * (STRIDE / 4) + W - 1] == bg(W - 1, H - 1);
        snprintf(w, sizeof w, "  fill the test pattern through the mapping (%llu ms, read back %s)",
                 (unsigned long long)(GetTickCount64() - t0), ok ? "matches" : "MISMATCH");
        if (step(w, ok ? 0 : -EIO))
            goto cleanup;

        if (export_to_gem(c, dev, drm_fd, b))
            goto cleanup;
    }

    /* Present. */
    {
        const ULONGLONG start = GetTickCount64(), end = start + (ULONGLONG)seconds * 1000;
        ULONGLONG next_report = start + 5000;
        uint64_t seq = 0, flips_ok = 0, flips_bad = 0;
        int last_err = 0;
        for (uint32_t k = 0;; k++) {
            struct buf *b = &bufs[k % NBUF];
            uint32_t x, y;
            square_at(k, &x, &y);
            if (b->has_sq)
                rect(b->cpu, b->sq_x, b->sq_y, 0, k);
            rect(b->cpu, x, y, 1, k);
            b->has_sq = 1;
            b->sq_x = x;
            b->sq_y = y;

            const struct crm_scanout_flip f = {
                .scanout = 0,
                .owner_handle = (uint32_t)drm_fd,
                .host_handle = b->gem,
                .width = W,
                .height = H,
                .stride = STRIDE,
                .offset = 0,
                .fourcc = DRM_FORMAT_XRGB8888,
                .modifier = DRM_FORMAT_MOD_LINEAR,
                .seq = ++seq,
            };
            r = crm_win_scanout_flip(&f);
            if (k == 0) {
                snprintf(w, sizeof w, "ScanoutFlip (owner %d, GEM %u, %ux%u XRGB8888 linear, seq 1)",
                         drm_fd, b->gem, W, H);
                if (step(w, r))
                    break;
                printf("       holding for %u s, re-flipping every %u ms over %d buffers\n",
                       seconds, frame_ms, NBUF);
                fflush(stdout);
            }
            if (r == 0) {
                flips_ok++;
            } else {
                flips_bad++;
                last_err = r;
            }
            const ULONGLONG now = GetTickCount64();
            if (now >= next_report) {
                printf("       %llu s: %" PRIu64 " flips ok, %" PRIu64 " failed%s%s\n",
                       (unsigned long long)((now - start) / 1000), flips_ok, flips_bad,
                       last_err ? ", last error " : "", last_err ? crm_status_name(last_err) : "");
                fflush(stdout);
                next_report += 5000;
            }
            if (now >= end)
                break;
            Sleep(frame_ms);
        }
        if (seq > 1) {
            snprintf(w, sizeof w, "%" PRIu64 " flips in %llu ms, %" PRIu64 " failed", flips_ok,
                     (unsigned long long)(GetTickCount64() - start), flips_bad);
            step(w, flips_bad ? (last_err ? last_err : -EIO) : 0);
        }
    }

cleanup:
    for (int i = 0; i < NBUF; i++) {
        struct buf *b = &bufs[i];
        if (b->gem) {
            struct { uint32_t handle, pad; } gc = { b->gem, 0 };
            snprintf(w, sizeof w, "buffer %d: DRM_IOCTL_GEM_CLOSE %u", i, b->gem);
            step(w, crm_win_ioctl(drm_fd, DRM_IOCTL_GEM_CLOSE, &gc, sizeof gc, NULL, 0));
        }
        if (b->cpu) {
            snprintf(w, sizeof w, "buffer %d: CPU-unmap", i);
            step(w, crm_unmap_memory(c, dev, b->mem, (void *)b->cpu, b->size, 0));
        }
        if (b->mem) {
            snprintf(w, sizeof w, "buffer %d: free", i);
            step(w, crm_free(c, dev, b->mem));
        }
    }
out:
    if (drm_fd >= 0) {
        crm_win_close_device(drm_fd);
        printf("[ ok ] close DRM render node\n");
    }
    if (sub) step("free subdevice", crm_free(c, dev, sub));
    if (dev) step("free device", crm_free(c, crm_root(c), dev));
    printf("       objects still tracked: %zu, CPU mappings: %zu\n", crm_object_count(c),
           crm_mapping_count(c));
    crm_close(c);
    printf("[ ok ] crm_close\n");
    printf("%s\n", failed ? "SCANOUT SMOKE FAILED" : "SCANOUT SMOKE PASSED");
    return failed ? 1 : 0;
}

#else /* !_WIN32 */

/* On Linux the guest kernel module sends the DRM ioctls and ScanoutFlip; this
 * program is the Windows route. Exit 77: skipped. */
int main(void)
{
    printf("crm_scanout_smoke: Windows only (the Linux guest presents from its kernel)\n");
    return 77;
}

#endif
