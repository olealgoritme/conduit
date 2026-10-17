/* SPDX-License-Identifier: MIT */
/*
 * rmtrace: LD_PRELOAD shim that logs NVIDIA RM escapes (ioctl on /dev/nvidiactl,
 * /dev/nvidiaN) of the process it is loaded into.  Black-box observation of the
 * public kernel ABI; struct layouts from the open kernel modules' SDK headers
 * (src/common/sdk/nvidia/inc/nvos.h, alloc/alloc_channel.h).
 *
 *   RMTRACE_LOG=/path/file   log file (default stderr)
 *   RMTRACE_ALL=1            also log every other NVIDIA escape (map/unmap etc.)
 *   RMTRACE_DUMP=<bytes>     max param bytes dumped (default 512)
 *
 * Exports rmtrace_mark(const char *) so a test program can put markers in the log.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#include "names.h"

#define NV_IOCTL_MAGIC 'F'
#define NV_IOCTL_BASE 200
#define NV_ESC_IOCTL_XFER_CMD (NV_IOCTL_BASE + 11)
#define NV_ESC_RM_FREE 0x29
#define NV_ESC_RM_CONTROL 0x2A
#define NV_ESC_RM_ALLOC 0x2B

typedef struct { uint32_t hRoot, hObjectParent, hObjectNew, hClass; uint64_t pAllocParms; uint32_t paramsSize, status; } NVOS21;
typedef struct { uint32_t hRoot, hObjectParent, hObjectNew, hClass; uint64_t pAllocParms, pRightsRequested; uint32_t paramsSize, flags, status; } NVOS64;
typedef struct { uint32_t hClient, hObject, cmd, flags; uint64_t params; uint32_t paramsSize, status; } NVOS54;
typedef struct { uint32_t hRoot, hObjectParent, hObjectOld, status; } NVOS00;
typedef struct { uint32_t cmd, size; uint64_t ptr; } nv_ioctl_xfer;

_Static_assert(sizeof(NVOS21) == 32, "os21");
_Static_assert(sizeof(NVOS64) == 48, "os64");
_Static_assert(sizeof(NVOS54) == 32, "os54");

static int (*real_ioctl)(int, unsigned long, ...);
static FILE *logf;
static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;
static int log_all;
static unsigned dump_max = 512;
static struct timespec t0;
static unsigned long seq;

/* handle -> class, so controls and frees can be named */
#define HMAX 65536
static struct { uint32_t h, cls; } htab[HMAX];
static void hput(uint32_t h, uint32_t cls)
{
    unsigned i = (h * 2654435761u) & (HMAX - 1);
    for (unsigned n = 0; n < HMAX; n++, i = (i + 1) & (HMAX - 1))
        if (htab[i].h == 0 || htab[i].h == h) { htab[i].h = h; htab[i].cls = cls; return; }
}
static int hget(uint32_t h, uint32_t *cls)
{
    unsigned i = (h * 2654435761u) & (HMAX - 1);
    for (unsigned n = 0; n < HMAX; n++, i = (i + 1) & (HMAX - 1)) {
        if (htab[i].h == 0) return 0;
        if (htab[i].h == h) { *cls = htab[i].cls; return 1; }
    }
    return 0;
}

static const char *clsname(uint32_t c)
{
    unsigned lo = 0, hi = sizeof cls_names / sizeof cls_names[0];
    while (lo < hi) { unsigned m = (lo + hi) / 2; if (cls_names[m].v == c) return cls_names[m].n; if (cls_names[m].v < c) lo = m + 1; else hi = m; }
    return "?";
}
static const char *ctlname(uint32_t c)
{
    unsigned lo = 0, hi = sizeof ctl_names / sizeof ctl_names[0];
    while (lo < hi) { unsigned m = (lo + hi) / 2; if (ctl_names[m].v == c) return ctl_names[m].n; if (ctl_names[m].v < c) lo = m + 1; else hi = m; }
    return "?";
}

static void init(void)
{
    if (real_ioctl) return;
    real_ioctl = (int (*)(int, unsigned long, ...))dlsym(RTLD_NEXT, "ioctl");
    const char *p = getenv("RMTRACE_LOG");
    if (p && *p) {
        char path[4096];
        snprintf(path, sizeof path, "%s", p);
        logf = fopen(path, "a");
    }
    if (!logf) logf = stderr;
    setvbuf(logf, NULL, _IOLBF, 0);
    log_all = getenv("RMTRACE_ALL") && atoi(getenv("RMTRACE_ALL"));
    if (getenv("RMTRACE_DUMP")) dump_max = (unsigned)atoi(getenv("RMTRACE_DUMP"));
    clock_gettime(CLOCK_MONOTONIC, &t0);
}

__attribute__((constructor)) static void ctor(void)
{
    init();
    char exe[512] = "";
    ssize_t n = readlink("/proc/self/exe", exe, sizeof exe - 1);
    if (n > 0) exe[n] = 0;
    fprintf(logf, "# rmtrace pid %d exe %s\n", getpid(), exe);
}

static double now_ms(void)
{
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (t.tv_sec - t0.tv_sec) * 1e3 + (t.tv_nsec - t0.tv_nsec) / 1e6;
}

void rmtrace_mark(const char *s)
{
    init();
    pthread_mutex_lock(&mu);
    fprintf(logf, "%10.3f ==== MARK %s\n", now_ms(), s);
    pthread_mutex_unlock(&mu);
}

/* 0 = not an NVIDIA device fd, 1 = nvidiactl, 2 = nvidiaN, 3 = other nvidia (uvm, modeset) */
static int fdkind(int fd)
{
    static signed char cache[4096];
    static int gen[4096];
    if (fd >= 0 && fd < 4096 && cache[fd]) return cache[fd] - 1;
    char lnk[64], dst[256];
    snprintf(lnk, sizeof lnk, "/proc/self/fd/%d", fd);
    ssize_t n = readlink(lnk, dst, sizeof dst - 1);
    int k = 0;
    if (n > 0) {
        dst[n] = 0;
        if (!strcmp(dst, "/dev/nvidiactl")) k = 1;
        else if (!strncmp(dst, "/dev/nvidia", 11) && dst[11] >= '0' && dst[11] <= '9') k = 2;
        else if (!strncmp(dst, "/dev/nvidia", 11)) k = 3;
    }
    (void)gen;
    /* fds get reused; do not cache negative results, cache positives only */
    if (k && fd >= 0 && fd < 4096) cache[fd] = (signed char)(k + 1);
    return k;
}

static void hexdump(const void *p, uint32_t size)
{
    const uint8_t *b = p;
    uint32_t n = size < dump_max ? size : dump_max;
    for (uint32_t i = 0; i < n; i += 16) {
        fprintf(logf, "            %04x:", i);
        for (uint32_t j = i; j < i + 16 && j < n; j++)
            fprintf(logf, "%s%02x", (j % 4) ? "" : " ", b[j]);
        fprintf(logf, "\n");
    }
    if (n < size) fprintf(logf, "            ... (%u more bytes)\n", size - n);
}

static const uint32_t *U32(const void *p, size_t off) { return (const uint32_t *)((const uint8_t *)p + off); }
static uint64_t U64(const void *p, size_t off) { uint64_t v; memcpy(&v, (const uint8_t *)p + off, 8); return v; }

static int interesting_class(uint32_t c)
{
    if (c == 0x9067 || c == 0xa06c) return 1;
    if ((c & 0xff) == 0x6f && c >= 0x906f) return 1;        /* *_CHANNEL_GPFIFO_* */
    if ((c & 0xff) == 0xc0 || (c & 0xff) == 0x97 || (c & 0xff) == 0xb5) return 1; /* compute, 3D, copy */
    if (!strcmp(clsname(c), "?")) return 1;
    return 0;
}

static void decode_alloc_params(uint32_t cls, const void *p, uint32_t sz, int post)
{
    if (!p) return;
    if (cls == 0x9067 && sz >= 12) {
        uint32_t f = *U32(p, 4);
        static const char *fl[] = { "SYNC", "ASYNC", "SPECIFIED", "ASYNC_PREFER_LOWER" };
        fprintf(logf, "         ctxshare: hVASpace 0x%x flags 0x%x (%s) subctxId %s 0x%x", *U32(p, 0), f, fl[f & 3],
                post ? "OUT" : "IN", *U32(p, 8));
        if (post) fprintf(logf, " -> VEID %u%s", *U32(p, 8) & 0x7fffffffu,
                          (f & 3) == 3 ? ((*U32(p, 8) >> 31) ? " (lower: SUCCESS)" : " (lower: FAIL)") : "");
        fprintf(logf, "\n");
    } else if (((cls & 0xff) == 0xc0 || (cls & 0xff) == 0x97) && sz >= 16 && !post) {
        fprintf(logf, "         gr obj (NV_GR_ALLOCATION_PARAMETERS?): version %u flags 0x%x size %u caps 0x%x\n",
                *U32(p, 0), *U32(p, 4), *U32(p, 8), *U32(p, 12));
    } else if ((cls & 0xff) == 0xb5 && sz >= 8 && !post) {
        fprintf(logf, "         copy obj: version %u engineType 0x%x\n", *U32(p, 0), *U32(p, 4));
    } else if (cls == 0xa06c && sz >= 20 && !post) {
        fprintf(logf, "         tsg: hObjectError 0x%x hObjectEccError 0x%x hVASpace 0x%x engineType 0x%x bIsCallingContextVgpuPlugin %u\n",
                *U32(p, 0), *U32(p, 4), *U32(p, 8), *U32(p, 12), *(const uint8_t *)U32(p, 16));
    } else if ((cls & 0xff) == 0x6f && cls >= 0x906f && sz >= 0x98 && !post) {
        /* NV_CHANNEL_ALLOC_PARAMS (615.78 layout; 376 bytes) */
        fprintf(logf, "         chan: hObjectError 0x%x hObjectBuffer 0x%x gpFifoOffset 0x%llx gpFifoEntries %u flags 0x%x\n",
                *U32(p, 0), *U32(p, 4), (unsigned long long)U64(p, 8), *U32(p, 16), *U32(p, 20));
        fprintf(logf, "               hContextShare 0x%x hVASpace 0x%x hHandleVASpace 0x%x hUserdMemory[0] 0x%x userdOffset[0] 0x%llx\n",
                *U32(p, 24), *U32(p, 28), *U32(p, 32), *U32(p, 36), (unsigned long long)U64(p, 72));
        fprintf(logf, "               engineType 0x%x cid %u subDeviceId 0x%x hObjectEccError 0x%x\n",
                *U32(p, 136), *U32(p, 140), *U32(p, 144), *U32(p, 148));
        if (sz >= 376)
            fprintf(logf, "               hPhysChannelGroup 0x%x internalFlags 0x%x ProcessID %u SubProcessID %u tpcConfigID %u\n",
                    *U32(p, 248), *U32(p, 252), *U32(p, 304), *U32(p, 308), *U32(p, 368));
    }
}

/* RM ignores NVOS21/64 paramsSize and copies the per-class size from its
 * resource list (alloc_free.c serverAllocApiCopyIn -> rmapiParamsCopyInit);
 * userspace often passes 0.  Sizes of the param structs (615.78 headers). */
static uint32_t class_param_size(uint32_t c)
{
    if (c == 0x9067) return 12;                                  /* NV_CTXSHARE_ALLOCATION_PARAMETERS */
    if (c == 0xa06c || c == 0xa16c || c == 0xb06c) return 20;    /* NV_CHANNEL_GROUP_ALLOCATION_PARAMETERS */
    if ((c & 0xff) == 0x6f && c >= 0x906f) return 376;           /* NV_CHANNEL_ALLOC_PARAMS */
    if ((c & 0xff) == 0xc0 || (c & 0xff) == 0x97) return 16;     /* NV_GR_ALLOCATION_PARAMETERS */
    if ((c & 0xff) == 0xb5) return 8;                            /* NVB0B5_ALLOCATION_PARAMETERS */
    return 0;
}

static void log_alloc(const char *dev, uint32_t hRoot, uint32_t par, uint32_t hNew, uint32_t cls, void *params,
                      uint32_t psz, uint32_t esz, uint32_t flags, uint32_t status, int ret, const void *pre, int os64)
{
    uint32_t pcls = 0;
    int haspar = hget(par, &pcls);
    if (status == 0 && ret == 0) hput(hNew, cls);
    fprintf(logf, "%10.3f #%lu %s ALLOC%s client 0x%x parent 0x%x(%s) new 0x%x class 0x%x %s psz %u%s%s -> status 0x%x%s\n",
            now_ms(), seq, dev, os64 ? "64" : "21", hRoot, par, haspar ? clsname(pcls) : "-", hNew, cls, clsname(cls), psz,
            params ? "" : " (NULL params)", flags ? " FINN" : "", status, ret ? " (ioctl failed)" : "");
    if (pre && esz && interesting_class(cls)) {
        fprintf(logf, "         params IN (%u bytes%s):\n", esz, psz ? "" : ", class size");
        hexdump(pre, esz);
        decode_alloc_params(cls, pre, esz, 0);
    }
    if (params && esz && cls == 0x9067)
        decode_alloc_params(cls, params, esz, 1);
}

static void decode_ctl(uint32_t cmd, const void *p, uint32_t sz, int post)
{
    if (!p) return;
    switch (cmd) {
    case 0xa06f0104: /* NVA06F_CTRL_CMD_BIND */
        if (!post && sz >= 4) fprintf(logf, "         BIND engineType 0x%x\n", *U32(p, 0));
        break;
    case 0xa06c0101: case 0xa06f0103:
        if (!post && sz >= 3) fprintf(logf, "         SCHEDULE bEnable %u bSkipSubmit %u bSkipEnable %u\n",
                                      ((const uint8_t *)p)[0], ((const uint8_t *)p)[1], ((const uint8_t *)p)[2]);
        break;
    case 0xc36f0108:
        if (post && sz >= 4) {
            uint32_t t = *U32(p, 0);
            fprintf(logf, "         WORK_SUBMIT_TOKEN 0x%08x (chid %u, runlist %u)\n", t, t & 0xfff, (t >> 16) & 0x7f);
        }
        break;
    case 0xc36f010a:
        if (!post && sz >= 4) fprintf(logf, "         TOKEN_NOTIF_INDEX %u\n", *U32(p, 0));
        break;
    case 0xa06c0103:
        if (!post && sz >= 8) fprintf(logf, "         TIMESLICE %llu us\n", (unsigned long long)U64(p, 0));
        break;
    case 0xa06c0107:
        if (!post && sz >= 4) fprintf(logf, "         INTERLEAVE_LEVEL %u\n", *U32(p, 0));
        break;
    }
}

static int dump_ctl(uint32_t cmd, uint32_t objcls)
{
    uint32_t c = cmd >> 16;
    if (c == 0x9067 || c == 0xa06c || c == 0xa06f || c == 0xc36f || c == 0x906f || c == 0x2080 + 0 * 1) {
        if (c == 0x2080) return (cmd & 0xff00) == 0x1100 || (cmd & 0xff00) == 0x1200; /* FIFO, GR */
        return 1;
    }
    if ((objcls & 0xff) == 0x6f || objcls == 0xa06c || objcls == 0x9067) return 1;
    if (c == 0x0080) return (cmd & 0xff00) == 0x1100 || (cmd & 0xff00) == 0x1700; /* NV0080 GR/FIFO */
    return 0;
}

static void log_control(const char *dev, NVOS54 *a, const void *pre, uint32_t status, int ret)
{
    uint32_t ocls = 0;
    int has = hget(a->hObject, &ocls);
    fprintf(logf, "%10.3f #%lu %s CONTROL client 0x%x obj 0x%x(%s) cmd 0x%08x %s psz %u%s -> status 0x%x%s\n", now_ms(),
            seq, dev, a->hClient, a->hObject, has ? clsname(ocls) : "-", a->cmd, ctlname(a->cmd), a->paramsSize,
            (a->flags & 4) ? " FINN" : "", status, ret ? " (ioctl failed)" : "");
    if (dump_ctl(a->cmd, ocls) && a->paramsSize) {
        if (pre) {
            fprintf(logf, "         params IN:\n");
            hexdump(pre, a->paramsSize);
            decode_ctl(a->cmd, pre, a->paramsSize, 0);
        }
        if (a->params) {
            fprintf(logf, "         params OUT:\n");
            hexdump((const void *)(uintptr_t)a->params, a->paramsSize);
            decode_ctl(a->cmd, (const void *)(uintptr_t)a->params, a->paramsSize, 1);
        }
    }
}

static int do_rm(int fd, unsigned long req, void *arg, unsigned nr, uint32_t size, void *argp_for_ioctl)
{
    int kind = fdkind(fd);
    const char *dev = kind == 1 ? "ctl" : kind == 2 ? "gpu" : "nv?";
    void *pre = NULL;
    uint32_t psz = 0;
    int r;

    if (nr == NV_ESC_RM_ALLOC && (size == sizeof(NVOS21) || size == sizeof(NVOS64))) {
        NVOS21 *a21 = arg;
        NVOS64 *a64 = arg;
        int os64 = size == sizeof(NVOS64);
        void *pp = (void *)(uintptr_t)(os64 ? a64->pAllocParms : a21->pAllocParms);
        psz = os64 ? a64->paramsSize : a21->paramsSize;
        uint32_t cls = os64 ? a64->hClass : a21->hClass;
        uint32_t esz = psz ? psz : class_param_size(cls);
        if (pp && esz && esz < (1u << 20)) { pre = malloc(esz); memcpy(pre, pp, esz); }
        r = real_ioctl(fd, req, argp_for_ioctl);
        pthread_mutex_lock(&mu);
        seq++;
        if (os64)
            log_alloc(dev, a64->hRoot, a64->hObjectParent, a64->hObjectNew, a64->hClass, pp, psz, esz, a64->flags, a64->status, r, pre, 1);
        else
            log_alloc(dev, a21->hRoot, a21->hObjectParent, a21->hObjectNew, a21->hClass, pp, psz, esz, 0, a21->status, r, pre, 0);
        pthread_mutex_unlock(&mu);
        free(pre);
        return r;
    }
    if (nr == NV_ESC_RM_CONTROL && size == sizeof(NVOS54)) {
        NVOS54 *a = arg;
        void *pp = (void *)(uintptr_t)a->params;
        if (pp && a->paramsSize && a->paramsSize < (1u << 20)) { pre = malloc(a->paramsSize); memcpy(pre, pp, a->paramsSize); }
        r = real_ioctl(fd, req, argp_for_ioctl);
        pthread_mutex_lock(&mu);
        seq++;
        log_control(dev, a, pre, a->status, r);
        pthread_mutex_unlock(&mu);
        free(pre);
        return r;
    }
    if (nr == NV_ESC_RM_FREE && size == sizeof(NVOS00)) {
        NVOS00 *a = arg;
        uint32_t c = 0;
        int has = hget(a->hObjectOld, &c);
        r = real_ioctl(fd, req, argp_for_ioctl);
        pthread_mutex_lock(&mu);
        seq++;
        fprintf(logf, "%10.3f #%lu %s FREE client 0x%x parent 0x%x obj 0x%x(%s) -> status 0x%x\n", now_ms(), seq, dev,
                a->hRoot, a->hObjectParent, a->hObjectOld, has ? clsname(c) : "-", a->status);
        pthread_mutex_unlock(&mu);
        return r;
    }
    r = real_ioctl(fd, req, argp_for_ioctl);
    if (log_all) {
        pthread_mutex_lock(&mu);
        seq++;
        fprintf(logf, "%10.3f #%lu %s ESC 0x%02x size %u -> %d\n", now_ms(), seq, dev, nr, size, r);
        pthread_mutex_unlock(&mu);
    }
    return r;
}

int ioctl(int fd, unsigned long req, ...)
{
    va_list ap;
    va_start(ap, req);
    void *arg = va_arg(ap, void *);
    va_end(ap);
    init();
    if (_IOC_TYPE(req) != NV_IOCTL_MAGIC || !fdkind(fd))
        return real_ioctl(fd, req, arg);
    unsigned nr = _IOC_NR(req);
    if (nr == NV_ESC_IOCTL_XFER_CMD && arg) {
        nv_ioctl_xfer *x = arg;
        return do_rm(fd, req, (void *)(uintptr_t)x->ptr, x->cmd, x->size, arg);
    }
    return do_rm(fd, req, arg, nr, _IOC_SIZE(req), arg);
}
