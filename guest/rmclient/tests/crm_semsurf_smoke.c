/* SPDX-License-Identifier: MIT */
/*
 * crm_semsurf_smoke: RM semaphore-surface fences end to end on a Windows guest
 * (dxvk-on-nvk S4, guest/windows/docs/rm-fence-marker.md), with nothing but
 * librmclient's Windows transport.
 *
 *   crm_open -> device, subdevice -> FB_GET_SEMAPHORE_SURFACE_LAYOUT ->
 *   system memory for the semaphores (own pages as an OS descriptor, or
 *   NV01_MEMORY_SYSTEM) -> NV_SEMAPHORE_SURFACE over it -> host render node ->
 *   SEMSURF_FENCE_CTX_CREATE (0x54, slot 0) -> per round: SEMSURF_FENCE_CREATE
 *   (0x55) for the next value, EVENT_REGISTER on the fence handle (the KMD
 *   records it as ours, KMD 22.22.311+), NV_SEMAPHORE_SURFACE_CTRL_CMD_SET_VALUE
 *   from the CPU, and the time until the event fires -> Close.
 *
 * Then: a fence for a value already reached (fires at once: the KMD's early-fire
 * latch), and a fence that is never reached (fires after its timeout, value
 * unchanged). The GPU-release variant of the same path is NVK's
 * (vk_rmfence_test, guest/nvk-rm/windows).
 *
 * Usage: crm_semsurf_smoke [rounds=200] [dri_index=0] [mem=os|rm]
 * Exit 0 when every step passed, 1 otherwise, 77 when not on Windows.
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

#define NV_SEMAPHORE_SURFACE 0x000000dau
#define NV_SEMAPHORE_SURFACE_CTRL_CMD_SET_VALUE 0x00da0004u
#define NV2080_CTRL_CMD_FB_GET_SEMAPHORE_SURFACE_LAYOUT 0x20801352u

struct semsurf_alloc {
    uint32_t h_semaphore_mem;
    uint32_t h_max_submitted_mem;
    uint64_t flags;
};
struct semsurf_set_value {
    uint64_t index;
    uint64_t new_value;
};
struct semsurf_layout {
    uint64_t max_submitted_offset;
    uint64_t monitored_fence_threshold_offset;
    uint64_t size;
    uint32_t caps;
    uint32_t pad;
};
_Static_assert(sizeof(struct semsurf_alloc) == 16, "NV_SEMAPHORE_SURFACE_ALLOC_PARAMETERS");
_Static_assert(sizeof(struct semsurf_layout) == 32, "GET_SEMAPHORE_SURFACE_LAYOUT");

#define MEM_SIZE 4096u

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

static LARGE_INTEGER qpf;
static double now_us(void)
{
    LARGE_INTEGER t;
    QueryPerformanceCounter(&t);
    return (double)t.QuadPart * 1e6 / (double)qpf.QuadPart;
}

static int cmp_d(const void *a, const void *b)
{
    const double x = *(const double *)a, y = *(const double *)b;
    return x < y ? -1 : x > y;
}

static void stats(const char *what, double *v, unsigned n)
{
    if (n == 0)
        return;
    qsort(v, n, sizeof(*v), cmp_d);
    double sum = 0;
    for (unsigned i = 0; i < n; i++)
        sum += v[i];
    printf("       %-38s n=%u min %.1f  median %.1f  p90 %.1f  p99 %.1f  max %.1f  mean %.1f us\n",
           what, n, v[0], v[n / 2], v[(n * 9) / 10], v[(n * 99) / 100], v[n - 1], sum / n);
}

int main(int argc, char **argv)
{
    const unsigned rounds = argc > 1 ? (unsigned)strtoul(argv[1], NULL, 0) : 200;
    const unsigned dri = argc > 2 ? (unsigned)strtoul(argv[2], NULL, 0) : 0;
    const int os_mem = !(argc > 3 && strcmp(argv[3], "rm") == 0);
    QueryPerformanceFrequency(&qpf);

    crm_client *c = NULL;
    if (step("crm_open", crm_open(&c, NULL)))
        return 1;
    printf("       RM version %s, root client 0x%08x\n", crm_rm_version(c), crm_root(c));

    uint64_t ops = 0;
    uint32_t features = 0;
    step("QUERY_CAPS", crm_win_caps(&ops, &features));
    printf("       supported_ops 0x%016" PRIx64 " (CAP_SCANOUT_FENCE %d, CAP_PRESENT_FENCE %d), "
           "device_features 0x%08x (DRM_FENCES %d)\n", ops, !!(ops & CRM_WIN_CAP_SCANOUT_FENCE),
           !!(ops & CRM_WIN_CAP_PRESENT_FENCE), features, !!(features & (1u << 11)));

    uint32_t dev = 0, sub = 0, mem = 0, semsurf = 0, ctx = 0;
    void *pages = NULL;
    volatile uint64_t *sem = NULL;
    int drm_fd = -1;
    char w[256];

    NV0080_ALLOC_PARAMETERS dp;
    memset(&dp, 0, sizeof dp);
    if (step("alloc NV01_DEVICE_0", crm_alloc(c, crm_root(c), &dev, NV01_DEVICE_0, &dp, sizeof dp)))
        goto out;
    NV2080_ALLOC_PARAMETERS sp = { .subDeviceId = 0 };
    if (step("alloc NV20_SUBDEVICE_0", crm_alloc(c, dev, &sub, NV20_SUBDEVICE_0, &sp, sizeof sp)))
        goto out;

    struct semsurf_layout lay;
    memset(&lay, 0, sizeof lay);
    if (step("NV2080_CTRL_CMD_FB_GET_SEMAPHORE_SURFACE_LAYOUT",
             crm_control(c, sub, NV2080_CTRL_CMD_FB_GET_SEMAPHORE_SURFACE_LAYOUT, &lay,
                         sizeof lay)))
        goto out;
    printf("       entry size %" PRIu64 ", max-submitted offset %" PRIu64 ", monitored-fence "
           "offset %" PRIu64 ", caps 0x%x (64-bit %d, monitored fence %d)\n", lay.size,
           lay.max_submitted_offset, lay.monitored_fence_threshold_offset, lay.caps,
           !!(lay.caps & 2), !!(lay.caps & 1));

    if (os_mem) {
        if (step("crm_alloc_pages 4 KiB", crm_alloc_pages(c, MEM_SIZE, &pages)))
            goto out;
        memset(pages, 0, MEM_SIZE);
        if (step("alloc NV01_MEMORY_SYSTEM_OS_DESCRIPTOR (semaphore memory)",
                 crm_alloc_os_descriptor(c, dev, &mem, pages, MEM_SIZE, 0)))
            goto out;
        sem = pages;
    } else {
        NV_MEMORY_ALLOCATION_PARAMS mp;
        memset(&mp, 0, sizeof mp);
        mp.owner = crm_root(c);
        mp.type = NVOS32_TYPE_IMAGE;
        mp.attr = (NVOS32_ATTR_LOCATION_PCI << NVOS32_ATTR_LOCATION_SHIFT) |
                  (NVOS32_ATTR_COHERENCY_CACHED << NVOS32_ATTR_COHERENCY_SHIFT);
        mp.size = MEM_SIZE;
        if (step("alloc NV01_MEMORY_SYSTEM (semaphore memory)",
                 crm_alloc(c, dev, &mem, NV01_MEMORY_SYSTEM, &mp, sizeof mp)))
            goto out;
        void *p = NULL;
        if (step("CPU-map it", crm_map_memory(c, dev, mem, 0, MEM_SIZE, 0, &p)))
            goto out;
        sem = p;
    }

    struct semsurf_alloc sa = { .h_semaphore_mem = mem };
    if (step("alloc NV_SEMAPHORE_SURFACE over it (under the subdevice)",
             crm_alloc(c, sub, &semsurf, NV_SEMAPHORE_SURFACE, &sa, sizeof sa)))
        goto out;
    printf("       semaphore surface 0x%08x, slot 0 = %" PRIu64 "\n", semsurf, sem[0]);

    int r = crm_win_open_device(CRM_WIN_DEV_DRI_BASE + dri, &drm_fd);
    snprintf(w, sizeof w, "open DRM render node %u -> handle %d", dri, drm_fd);
    if (step(w, r))
        goto out;

    r = crm_win_semsurf_ctx_create(drm_fd, crm_root(c), semsurf, MEM_SIZE, 0, &ctx);
    snprintf(w, sizeof w, "SEMSURF_FENCE_CTX_CREATE (slot 0) -> context GEM %u", ctx);
    if (step(w, r))
        goto out;

    /* Rounds: create a fence for value v, register its event, signal from the
     * CPU through RM, wait. */
    double *t_create = calloc(rounds, sizeof(double));
    double *t_signal = calloc(rounds, sizeof(double));
    double *t_fire = calloc(rounds, sizeof(double));
    double *t_close = calloc(rounds, sizeof(double));
    unsigned n = 0, timeouts = 0, early = 0;
    uint64_t v = 0;
    for (unsigned i = 0; i < rounds; i++) {
        v++;
        int fence = -1;
        double a = now_us();
        r = crm_win_semsurf_fence_create(drm_fd, ctx, v, 2000, &fence);
        double b = now_us();
        if (r) {
            snprintf(w, sizeof w, "round %u: SEMSURF_FENCE_CREATE(%" PRIu64 ")", i, v);
            step(w, r);
            break;
        }
        /* Register now (before the signal), so the fire is a real wake. */
        const int pre = crm_win_fence_wait(fence, 0);
        if (pre != 0) {
            if (pre == 1)
                early++;
            else {
                snprintf(w, sizeof w, "round %u: EVENT_REGISTER on fence %d", i, fence);
                step(w, pre);
                crm_win_close_device(fence);
                break;
            }
        }
        struct semsurf_set_value sv = { .index = 0, .new_value = v };
        double c0 = now_us();
        r = crm_control(c, semsurf, NV_SEMAPHORE_SURFACE_CTRL_CMD_SET_VALUE, &sv, sizeof sv);
        double c1 = now_us();
        if (r) {
            step("NV_SEMAPHORE_SURFACE_CTRL_CMD_SET_VALUE", r);
            crm_win_close_device(fence);
            break;
        }
        const int fired = crm_win_fence_wait(fence, 3000);
        double d = now_us();
        if (fired != 1)
            timeouts++;
        crm_win_close_device(fence);
        double e = now_us();
        t_create[n] = b - a;
        t_signal[n] = c1 - c0;
        t_fire[n] = d - c0;
        t_close[n] = e - d;
        n++;
    }
    snprintf(w, sizeof w, "%u rounds: fence fired after a CPU SET_VALUE (%u timed out, %u fired "
             "before the signal; slot 0 now %" PRIu64 ")", n, timeouts, early, sem[0]);
    step(w, (n == rounds && timeouts == 0 && early == 0 && sem[0] == v) ? 0 : -EIO);
    stats("SEMSURF_FENCE_CREATE round trip", t_create, n);
    stats("SET_VALUE control round trip", t_signal, n);
    stats("SET_VALUE start -> event fired", t_fire, n);
    stats("Close of the fence", t_close, n);

    /* Already reached: the fire may beat the create's reply; the KMD latches it. */
    {
        int fence = -1;
        r = crm_win_semsurf_fence_create(drm_fd, ctx, v, 2000, &fence);
        if (!step("fence for a value already reached", r)) {
            double a = now_us();
            const int fired = crm_win_fence_wait(fence, 1000);
            snprintf(w, sizeof w, "  ... fires at once (%.1f us, early-fire latch)", now_us() - a);
            step(w, fired == 1 ? 0 : -ETIMEDOUT);
            crm_win_close_device(fence);
        }
    }
    /* Never reached: fires after its timeout, the value unchanged. */
    {
        int fence = -1;
        r = crm_win_semsurf_fence_create(drm_fd, ctx, v + 1000, 200, &fence);
        if (!step("fence for a value never reached, 200 ms timeout", r)) {
            double a = now_us();
            const int fired = crm_win_fence_wait(fence, 7000);
            const double ms = (now_us() - a) / 1000.0;
            snprintf(w, sizeof w, "  ... fires after the timeout (%.1f ms), slot still %" PRIu64,
                     ms, sem[0]);
            step(w, fired == 1 && sem[0] == v && ms >= 100.0 ? 0 : -EIO);
            crm_win_close_device(fence);
        }
    }
    free(t_create);
    free(t_signal);
    free(t_fire);
    free(t_close);

out:
    if (drm_fd >= 0)
        crm_win_close_device(drm_fd); /* frees the fence context's GEM handle too */
    if (semsurf)
        crm_free(c, sub, semsurf);
    if (mem)
        crm_free(c, dev, mem);
    if (pages)
        crm_free_pages(c, pages, MEM_SIZE);
    if (sub)
        crm_free(c, dev, sub);
    if (dev)
        crm_free(c, crm_root(c), dev);
    crm_close(c);
    printf("%s\n", failed ? "FAIL" : "PASS");
    return failed ? 1 : 0;
}

#else
int main(void)
{
    printf("crm_semsurf_smoke: Windows only\n");
    return 77;
}
#endif
