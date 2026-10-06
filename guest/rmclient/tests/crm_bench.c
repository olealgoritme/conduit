/* SPDX-License-Identifier: MIT */
/*
 * crm_bench: latency of RM calls through librmclient, for comparing a host's
 * native /dev/nvidiactl with Conduit's forwarding path (Linux guest module or
 * the Windows KMD's HELIOS_ESCAPE_NVRM).
 *
 *   crm_bench [-n iterations] [-w warmup] [-o only-substring]
 *
 * Every case runs `warmup` untimed iterations, then `iterations` timed ones,
 * and prints p50/p90/p99/mean/min in microseconds and calls per second of the
 * timed unit, and "rm-calls", the RM escapes one unit makes (channel
 * open/close and the mmap itself not counted). Setup (device, subdevice, VA space, memory) is done once and is
 * not timed. Read-only for RM state outside this client: everything it
 * allocates lives under its own root client and is freed.
 *
 * On Windows, CRM_WIN_PROF_FILE=path also has librmclient write its
 * per-escape-kind timing table to `path` (src/transport_windows.c).
 *
 * Cases:
 *   escape.query_caps   (Windows only) HELIOS_NVRM_OP_QUERY_CAPS: answered in
 *                       the KMD, so the bare D3DKMTEscape cost
 *   win.open_close_ctl  (Windows only) Open + Close of a control channel
 *                       (host open()/close() of /dev/nvidiactl)
 *   fwd.backend_reject  NV_ESC_RM_GET_EVENT_DATA on the control channel: the
 *                       Conduit backend refuses it without calling nvidia.ko
 *                       (natively it reaches RM, which refuses it too)
 *   ctrl.timer_get_time NV2080_CTRL_CMD_TIMER_GET_TIME (8-byte params)
 *   ctrl.gpu_name       NV2080_CTRL_CMD_GPU_GET_NAME_STRING (68-byte params)
 *   ctrl.root_addr_space NV0000_CTRL_CMD_CLIENT_GET_ADDR_SPACE_TYPE on the root
 *                       client (no GPU lock)
 *   alloc.sysmem64k     NV01_MEMORY_SYSTEM 64 KiB alloc + free (2 RM calls)
 *   alloc.os_event      crm_event_open + crm_event_close (channel open,
 *                       ALLOC_OS_EVENT, alloc, free, FREE_OS_EVENT, close)
 *   map.cpu64k          crm_map_memory + crm_unmap_memory, 64 KiB sysmem
 *   event.roundtrip     SET_TRIGGER -> crm_event_wait -> crm_event_drain
 *                       (also split into its three parts; drain is one
 *                       GET_EVENT_DATA when one event is queued)
 *   map.dma2m           crm_map_dma + crm_unmap_dma, 2 MiB vidmem into a
 *                       FERMI_VASPACE_A (the library allocates/frees an
 *                       NV50_MEMORY_VIRTUAL per mapping: 4 RM calls)
 */
#define _POSIX_C_SOURCE 200809L /* clock_gettime under -std=c11 */
#include <errno.h>
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifdef _WIN32
#include <windows.h>
#else
#include <time.h>
#endif

#include "rmclient.h"
#include "rmclient_transport.h"
#include "nv_ioctl_defs.h"

#define KIB 1024ull
#define MIB (1024ull * 1024ull)

#define NV2080_CTRL_CMD_TIMER_GET_TIME 0x20800403u
typedef struct { uint64_t time_nsec; } TIMER_GET_TIME_PARAMS;

#define NV0000_CTRL_CMD_CLIENT_GET_ADDR_SPACE_TYPE 0x00000d01u
typedef struct {
    uint32_t hObject;
    uint32_t mapFlags;
    uint32_t addrSpaceType;
} CLIENT_GET_ADDR_SPACE_TYPE_PARAMS;

#define NV2080_CTRL_CMD_EVENT_SET_NOTIFICATION 0x20800301u
#define NV2080_CTRL_CMD_EVENT_SET_TRIGGER 0x20800302u
#define NV2080_NOTIFIERS_SW 0u
typedef struct {
    uint32_t event;
    uint32_t action;
    uint8_t bNotifyState;
    uint32_t info32;
    uint16_t info16;
} SET_NOTIFICATION_PARAMS;
_Static_assert(sizeof(SET_NOTIFICATION_PARAMS) == 20, "SET_NOTIFICATION");

/* ---- clock --------------------------------------------------------------- */

static double now_us(void)
{
#ifdef _WIN32
    static LARGE_INTEGER f;
    LARGE_INTEGER t;
    if (!f.QuadPart)
        QueryPerformanceFrequency(&f);
    QueryPerformanceCounter(&t);
    return (double)t.QuadPart * 1e6 / (double)f.QuadPart;
#else
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec * 1e6 + (double)ts.tv_nsec / 1e3;
#endif
}

/* ---- stats --------------------------------------------------------------- */

static int g_iters = 2000, g_warm = 50;
static const char *g_only;
static double *g_samples;

static int cmp_d(const void *a, const void *b)
{
    const double x = *(const double *)a, y = *(const double *)b;
    return x < y ? -1 : x > y;
}

static void report(const char *name, double *s, int n, int calls_per_iter)
{
    if (n <= 0) {
        printf("%-22s %8s\n", name, "-");
        return;
    }
    double sum = 0;
    for (int i = 0; i < n; i++)
        sum += s[i];
    qsort(s, (size_t)n, sizeof(*s), cmp_d);
    const double mean = sum / n;
    printf("%-22s %6d %9.1f %9.1f %9.1f %9.1f %9.1f %10.0f  %d\n", name, n, s[n / 2],
           s[(int)(n * 0.90)], s[(int)(n * 0.99)], mean, s[0], mean > 0 ? 1e6 / mean : 0,
           calls_per_iter);
    fflush(stdout);
}

static void skip(const char *name, const char *why, int r)
{
    printf("%-22s skipped: %s (%s %d)\n", name, why, crm_status_name(r), r);
    fflush(stdout);
}

static int wanted(const char *name)
{
    return !g_only || strstr(name, g_only);
}

/* Time `fn` g_iters times after g_warm untimed runs; stop at the first error. */
typedef int (*bench_fn)(void *ctx);
static void run(const char *name, bench_fn fn, void *ctx, int calls_per_iter)
{
    if (!wanted(name))
        return;
    for (int i = 0; i < g_warm; i++) {
        int r = fn(ctx);
        if (r) {
            skip(name, "failed in warmup", r);
            return;
        }
    }
    int n = 0;
    for (; n < g_iters; n++) {
        const double t0 = now_us();
        int r = fn(ctx);
        const double t1 = now_us();
        if (r) {
            skip(name, "failed", r);
            break;
        }
        g_samples[n] = t1 - t0;
    }
    report(name, g_samples, n, calls_per_iter);
}

/* ---- cases --------------------------------------------------------------- */

struct ctx {
    crm_client *c;
    uint32_t dev, sub, vas, sys64k, vid2m;
    uint32_t ev;
    int efd;
};

static int __attribute__((unused)) b_query_caps(void *v)
{
    (void)v;
    return crm_win_query_caps();
}

static int __attribute__((unused)) b_open_close(void *v)
{
    (void)v;
    int fd = -1;
    int r = crm_win_open_device(CRM_WIN_DEV_CTL, &fd);
    if (r)
        return r;
    crm_win_close_device(fd);
    return 0;
}

static int b_backend_reject(void *v)
{
    struct ctx *x = v;
    NvUnixEvent ue;
    NVOS41_PARAMETERS p;
    memset(&ue, 0, sizeof(ue));
    memset(&p, 0, sizeof(p));
    p.pEvent = (uint64_t)(uintptr_t)&ue;
    int r = crm_escape(x->c, -1, NV_ESC_RM_GET_EVENT_DATA, &p, sizeof(p));
    /* Conduit: -EINVAL from the backend. Native: 0 with an RM status. */
    return (r == -EINVAL || r == 0) ? 0 : r;
}

static int b_timer(void *v)
{
    struct ctx *x = v;
    TIMER_GET_TIME_PARAMS p = { 0 };
    return crm_control(x->c, x->sub, NV2080_CTRL_CMD_TIMER_GET_TIME, &p, sizeof(p));
}

static int b_name(void *v)
{
    struct ctx *x = v;
    NV2080_CTRL_GPU_GET_NAME_STRING_PARAMS p;
    memset(&p, 0, sizeof(p));
    p.gpuNameStringFlags = NV2080_CTRL_GPU_GET_NAME_STRING_FLAGS_TYPE_ASCII;
    return crm_control(x->c, x->sub, NV2080_CTRL_CMD_GPU_GET_NAME_STRING, &p, sizeof(p));
}

static int b_root(void *v)
{
    struct ctx *x = v;
    CLIENT_GET_ADDR_SPACE_TYPE_PARAMS p = { .hObject = x->sys64k };
    return crm_control(x->c, crm_root(x->c), NV0000_CTRL_CMD_CLIENT_GET_ADDR_SPACE_TYPE, &p,
                       sizeof(p));
}

static void sysmem_params(NV_MEMORY_ALLOCATION_PARAMS *mp, crm_client *c, uint64_t size)
{
    memset(mp, 0, sizeof(*mp));
    mp->owner = crm_root(c);
    mp->type = NVOS32_TYPE_IMAGE;
    mp->attr = (NVOS32_ATTR_LOCATION_PCI << NVOS32_ATTR_LOCATION_SHIFT) |
               (NVOS32_ATTR_PAGE_SIZE_4KB << NVOS32_ATTR_PAGE_SIZE_SHIFT) |
               (NVOS32_ATTR_PHYSICALITY_NONCONTIGUOUS << NVOS32_ATTR_PHYSICALITY_SHIFT) |
               (NVOS32_ATTR_COHERENCY_CACHED << NVOS32_ATTR_COHERENCY_SHIFT);
    mp->size = size;
}

static int b_alloc_sys(void *v)
{
    struct ctx *x = v;
    NV_MEMORY_ALLOCATION_PARAMS mp;
    sysmem_params(&mp, x->c, 64 * KIB);
    uint32_t h = 0;
    int r = crm_alloc(x->c, x->dev, &h, NV01_MEMORY_SYSTEM, &mp, sizeof(mp));
    if (r)
        return r;
    return crm_free(x->c, x->dev, h);
}

static int b_os_event(void *v)
{
    struct ctx *x = v;
    uint32_t ev = 0;
    int fd = -1;
    int r = crm_event_open(x->c, x->sub, NV2080_NOTIFIERS_SW, &ev, &fd);
    if (r)
        return r;
    return crm_event_close(x->c, ev, fd);
}

static int b_map_cpu(void *v)
{
    struct ctx *x = v;
    void *p = NULL;
    int r = crm_map_memory(x->c, x->dev, x->sys64k, 0, 64 * KIB, 0, &p);
    if (r)
        return r;
    ((volatile uint32_t *)p)[0] = 1;
    return crm_unmap_memory(x->c, x->dev, x->sys64k, p, 64 * KIB, 0);
}

static int b_map_dma(void *v)
{
    struct ctx *x = v;
    uint64_t va = 0;
    int r = crm_map_dma(x->c, x->dev, x->vas, x->vid2m, 0, 2 * MIB, 0, &va);
    if (r)
        return r;
    return crm_unmap_dma(x->c, x->dev, x->vas, x->vid2m, 0, va);
}

/* SET_TRIGGER -> wait -> drain, with the parts timed separately as well. */
static void bench_event(struct ctx *x)
{
    const char *name = "event.roundtrip";
    if (!wanted(name))
        return;
    int r = crm_event_open(x->c, x->sub, NV2080_NOTIFIERS_SW, &x->ev, &x->efd);
    if (r) {
        skip(name, "crm_event_open", r);
        return;
    }
    SET_NOTIFICATION_PARAMS sn;
    memset(&sn, 0, sizeof(sn));
    sn.event = NV2080_NOTIFIERS_SW;
    sn.action = 2; /* REPEAT */
    r = crm_control(x->c, x->sub, NV2080_CTRL_CMD_EVENT_SET_NOTIFICATION, &sn, sizeof(sn));
    if (r) {
        skip(name, "SET_NOTIFICATION", r);
        goto out;
    }
    double *trig = calloc((size_t)g_iters, sizeof(double));
    double *wait = calloc((size_t)g_iters, sizeof(double));
    double *drain = calloc((size_t)g_iters, sizeof(double));
    if (!trig || !wait || !drain)
        goto out_free;
    struct crm_event_data d[8];
    int n = 0;
    for (int i = -g_warm; i < g_iters; i++) {
        const double t0 = now_us();
        r = crm_control(x->c, x->sub, NV2080_CTRL_CMD_EVENT_SET_TRIGGER, NULL, 0);
        const double t1 = now_us();
        int w = r ? r : crm_event_wait(x->c, x->efd, 2000);
        const double t2 = now_us();
        int nd = w == 1 ? crm_event_drain(x->c, x->efd, d, 8) : -1;
        const double t3 = now_us();
        if (r || w != 1 || nd < 1) {
            printf("%-22s failed: trigger %d wait %d drain %d\n", name, r, w, nd);
            break;
        }
        if (i < 0)
            continue;
        g_samples[n] = t3 - t0;
        trig[n] = t1 - t0;
        wait[n] = t2 - t1;
        drain[n] = t3 - t2;
        n++;
    }
    report(name, g_samples, n, 2);
    report("  event.trigger", trig, n, 1);
    report("  event.wait", wait, n, 0);
    report("  event.drain", drain, n, 1);
out_free:
    free(trig);
    free(wait);
    free(drain);
    sn.action = 0;
    (void)crm_control(x->c, x->sub, NV2080_CTRL_CMD_EVENT_SET_NOTIFICATION, &sn, sizeof(sn));
out:
    crm_event_close(x->c, x->ev, x->efd);
}

/* ---- main ---------------------------------------------------------------- */

static int setup(struct ctx *x)
{
    crm_client *c = x->c;
    NV0080_ALLOC_PARAMETERS dp;
    memset(&dp, 0, sizeof(dp));
    int r = crm_alloc(c, crm_root(c), &x->dev, NV01_DEVICE_0, &dp, sizeof(dp));
    if (r)
        return printf("alloc NV01_DEVICE_0: %s\n", crm_status_name(r)), r;
    NV2080_ALLOC_PARAMETERS sp = { .subDeviceId = 0 };
    r = crm_alloc(c, x->dev, &x->sub, NV20_SUBDEVICE_0, &sp, sizeof(sp));
    if (r)
        return printf("alloc NV20_SUBDEVICE_0: %s\n", crm_status_name(r)), r;
    NV_VASPACE_ALLOCATION_PARAMETERS vp;
    memset(&vp, 0, sizeof(vp));
    r = crm_alloc(c, x->dev, &x->vas, FERMI_VASPACE_A, &vp, sizeof(vp));
    if (r)
        printf("alloc FERMI_VASPACE_A: %s (map.dma2m skipped)\n", crm_status_name(r));
    NV_MEMORY_ALLOCATION_PARAMS mp;
    sysmem_params(&mp, c, 64 * KIB);
    r = crm_alloc(c, x->dev, &x->sys64k, NV01_MEMORY_SYSTEM, &mp, sizeof(mp));
    if (r)
        printf("alloc NV01_MEMORY_SYSTEM 64 KiB: %s\n", crm_status_name(r));
    memset(&mp, 0, sizeof(mp));
    mp.owner = crm_root(c);
    mp.type = NVOS32_TYPE_IMAGE;
    mp.flags = NVOS32_ALLOC_FLAGS_ALIGNMENT_FORCE | NVOS32_ALLOC_FLAGS_IGNORE_BANK_PLACEMENT;
    mp.attr = (NVOS32_ATTR_LOCATION_VIDMEM << NVOS32_ATTR_LOCATION_SHIFT) |
              (NVOS32_ATTR_PAGE_SIZE_HUGE << NVOS32_ATTR_PAGE_SIZE_SHIFT) |
              (NVOS32_ATTR_PHYSICALITY_CONTIGUOUS << NVOS32_ATTR_PHYSICALITY_SHIFT);
    mp.size = 2 * MIB;
    mp.alignment = 2 * MIB;
    r = crm_alloc(c, x->dev, &x->vid2m, NV01_MEMORY_LOCAL_USER, &mp, sizeof(mp));
    if (r)
        printf("alloc NV01_MEMORY_LOCAL_USER 2 MiB: %s\n", crm_status_name(r));
    return 0;
}

int main(int argc, char **argv)
{
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "-n") && i + 1 < argc)
            g_iters = atoi(argv[++i]);
        else if (!strcmp(argv[i], "-w") && i + 1 < argc)
            g_warm = atoi(argv[++i]);
        else if (!strcmp(argv[i], "-o") && i + 1 < argc)
            g_only = argv[++i];
        else {
            fprintf(stderr, "usage: %s [-n iterations] [-w warmup] [-o case-substring]\n",
                    argv[0]);
            return 2;
        }
    }
    if (g_iters < 1)
        g_iters = 1;
    if (g_warm < 0)
        g_warm = 0;
    g_samples = calloc((size_t)g_iters, sizeof(double));
    if (!g_samples)
        return 1;

    struct ctx x;
    memset(&x, 0, sizeof(x));
    x.efd = -1;
    double t0 = now_us();
    int r = crm_open(&x.c, NULL);
    if (r) {
        printf("crm_open: %s (%d) %s\n", crm_status_name(r), r, crm_status_string(r));
        return 1;
    }
    printf("crm_bench: RM %s, transport %s, crm_open %.1f us, %d iterations (+%d warmup)\n",
           crm_rm_version(x.c), crm_default_transport()->name, now_us() - t0, g_iters, g_warm);
    if (setup(&x))
        goto out;

    printf("%-22s %6s %9s %9s %9s %9s %9s %10s  %s\n", "case (us)", "n", "p50", "p90", "p99",
           "mean", "min", "iter/s", "rm-calls");
#ifdef _WIN32
    run("escape.query_caps", b_query_caps, &x, 0);
    run("win.open_close_ctl", b_open_close, &x, 0);
#endif
    run("fwd.backend_reject", b_backend_reject, &x, 1);
    run("ctrl.timer_get_time", b_timer, &x, 1);
    run("ctrl.gpu_name", b_name, &x, 1);
    if (x.sys64k)
        run("ctrl.root_addr_space", b_root, &x, 1);
    run("alloc.sysmem64k", b_alloc_sys, &x, 2);
    run("alloc.os_event", b_os_event, &x, 4);
    if (x.sys64k)
        run("map.cpu64k", b_map_cpu, &x, 2);
    bench_event(&x);
    if (x.vas && x.vid2m)
        run("map.dma2m", b_map_dma, &x, 4);

out:
    if (x.vid2m) crm_free(x.c, x.dev, x.vid2m);
    if (x.sys64k) crm_free(x.c, x.dev, x.sys64k);
    if (x.vas) crm_free(x.c, x.dev, x.vas);
    if (x.sub) crm_free(x.c, x.dev, x.sub);
    if (x.dev) crm_free(x.c, crm_root(x.c), x.dev);
    printf("objects still tracked: %zu, CPU mappings: %zu\n", crm_object_count(x.c),
           crm_mapping_count(x.c));
    crm_close(x.c);
    free(g_samples);
    return 0;
}
