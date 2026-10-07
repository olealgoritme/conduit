/* SPDX-License-Identifier: MIT */
/*
 * crm_ce_copy_smoke: the windowed Present copy on an RM copy-engine (CE)
 * channel, in user mode (guest/windows/docs/rm-copy-engine-present.md, M1).
 *
 * Two RM clients in one process:
 *   producer: device, subdevice, a 4 KiB NV01_MEMORY_SYSTEM "producer
 *             timeline" (64-bit value at offset 0; an NV_SEMAPHORE_SURFACE
 *             over it when RM allows one) and the copy source (video memory
 *             filled through BAR1, or system memory), pitch layout.
 *   copier:   (stands in for the KMD) device, subdevice, VA space, a CE
 *             channel: KEPLER_CHANNEL_GROUP_A on the chosen engine,
 *             FERMI_CONTEXT_SHARE_A, the GPFIFO channel, BIND, the DMA copy
 *             object, work-submit token, schedule; the usermode doorbell.
 *             It duplicates the producer's semaphore memory and source
 *             (NV_ESC_RM_DUP_OBJECT) and GPU-maps them. The destination is
 *             an OS descriptor over ordinary process pages.
 *
 * Per round the push buffer is
 *   host SEM_EXECUTE ACQ_STRICT_GEQ on the producer value V (SWITCH_TSG),
 *   CE semaphore-only launch with timestamp (T0: the CE starts),
 *   CE pitch copy source -> destination, its semaphore with timestamp (T1),
 *   host SEM_EXECUTE RELEASE (WFI, 64-bit) of the completion value C,
 *   NON_STALL_INTERRUPT,
 * submitted with one GPFIFO entry, GP_PUT in USERD and the token written to
 * the doorbell. Stages:
 *   ready: V is set before the doorbell; doorbell -> C seen on the CPU
 *          (submit_to_done_acquire_satisfied_us).
 *   wait:  submitted first; after --hold-ms the CPU sets V (C must not have
 *          landed before); set -> C seen (acquire_satisfy_to_done_us).
 *   probe: a push of only a host release (no WFI); doorbell -> seen
 *          (doorbell_to_gpfifo_get_us: USERD GP_GET is not written back
 *          under GSP, nvk-rm patch 0009, so the first release stands in).
 *   copy_us: T1 - T0 from the CE's own GPU timestamps (ns, GPU timer).
 *   contended (--contend): a large copy on a second channel (the graphics
 *          CE by default) into another OS descriptor is in flight while the
 *          ready round runs; reports copy_us_contended next to copy_us.
 *
 * Every CPU wait is bounded (--timeout-ms). A wait that times out releases
 * the producer value (so an acquire cannot hold the GPU), reads the error
 * notifier and stops; teardown disables the TSG's schedule and frees it
 * (channel and CE object with it) before any memory it references.
 *
 * Method encodings, with the header each comes from:
 *   host (*6F) SEM_ADDR_LO..SEM_EXECUTE 0x5c..0x6c, OPERATION 2:0 (RELEASE 1,
 *   ACQ_STRICT_GEQ 2), ACQUIRE_SWITCH_TSG 12, RELEASE_WFI 20, PAYLOAD_SIZE
 *   24 (64BIT 1), NON_STALL_INTERRUPT 0x20, SET_OBJECT 0x0 (NVCLASS 15:0):
 *   clc56f.h, identical in Mesa and in the 610.57.04 open kernel modules
 *   (clca6f.h in both lists the same offsets for the fields it keeps); also
 *   nvk-rm patch 0002.
 *   CE (*B5) SET_SEMAPHORE_A/B/PAYLOAD 0x240/0x244/0x248, LAUNCH_DMA 0x300
 *   with DATA_TRANSFER_TYPE 1:0, FLUSH_ENABLE 2, SEMAPHORE_TYPE 4:3,
 *   SRC/DST_MEMORY_LAYOUT 7/8, MULTI_LINE_ENABLE 9, SRC/DST_TYPE 12/13,
 *   FLUSH_TYPE 25, SEMAPHORE_PAYLOAD_SIZE 27, OFFSET_IN_UPPER..LINE_COUNT
 *   0x400..0x41c: Mesa's clc7b5.h and clcab5.h, and the 610.57.04 open
 *   kernel modules' clc7b5.h (all three agree). The 610.57.04 clcab5.h only
 *   carries LAUNCH_DMA and DATA_TRANSFER_TYPE, which agree; the other
 *   0xcab5 fields are UNVERIFIED against that release's clcab5.h (it omits
 *   them) and taken as Mesa's clcab5.h has them.
 *   The timestamp is the second half of a 16-byte semaphore release
 *   (the 610.57.04 open kernel modules' uvm_push.c uvm_push_timestamp; the
 *   CE launch it times is uvm_hopper_ce.c semaphore_timestamp).
 *   Work-submit token: runlist id 22:16, channel id 11:0 (610.57.04
 *   dev_vm.h for GB202, dev_ctrl.h for GA100).
 *
 * Usage: crm_ce_copy_smoke [options]
 *   --gen gb202|ada        class set (default gb202: 0xca6f, 0xcab5, 0xc761;
 *                          ada: 0xc56f, 0xc7b5, 0xc561)
 *   --engine <n>|gr        CE instance COPY<n>, or the graphics engine's CE
 *                          (default: the first async CE the query reports)
 *   --iterations <n>       measured iterations (default 200)
 *   --duration <s>         run the measured loop this long instead
 *   --delay <s>            wait this long after setup before the loop
 *   --hold-ms <ms>         producer hold in the wait stage (default 2)
 *   --size <W>x<H>         copy W*H*4 bytes (default 1600x900)
 *   --src vid|sys          source in video (default) or system memory
 *   --contend              also run contended rounds
 *   --contend-engine <n>|gr  engine of the competing copy (default gr)
 *   --contend-mb <n>       size of the competing copy (default 64)
 *   --vas device|new       the device's VA space (default) or a new one
 *   --release cpu|semsurf  the CPU sets V by a store (default) or SET_VALUE
 *   --timeout-ms <ms>      bound of every CPU wait (default 2000)
 *   --fence                (Windows) also time doorbell -> RM fence event
 * Exit 0 on PASS, 1 on FAIL.
 */
#ifndef _WIN32
#define _POSIX_C_SOURCE 200809L
#endif
#include <errno.h>
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifdef _WIN32
#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#include <windows.h>
#else
#include <time.h>
#endif

#include "rmclient.h"
#include "rmclient_transport.h"
#include "nv_ioctl_defs.h"

/* ---- RM classes, controls and parameters (nvk-rm patches 0002, 0008,
 * 0030, 0035; sizes as the 610.57.04 allowlist checks them) ---- */

#define FERMI_CONTEXT_SHARE_A 0x00009067u
#define KEPLER_CHANNEL_GROUP_A 0x0000a06cu
#define NV_SEMAPHORE_SURFACE 0x000000dau

#define NV2080_ENGINE_TYPE_GRAPHICS 0x00000001u
#define NV2080_ENGINE_TYPE_COPY0 0x00000009u /* cl2080_notification.h */
#define NV2080_ENGINE_TYPE_COPY9 0x00000012u
#define NV2080_ENGINE_TYPE_COPY10 0x00000034u
#define NV2080_ENGINE_TYPE_COPY19 0x0000003du
#define NV2080_ENGINE_TYPE_COPY(i) ((i) < 10 ? NV2080_ENGINE_TYPE_COPY0 + (i) : NV2080_ENGINE_TYPE_COPY10 + (i) - 10)
#define NV2080_NOTIFIERS_FIFO_EVENT_MTHD 35u

#define NV_VASPACE_ALLOCATION_INDEX_GPU_DEVICE 3u
#define NV_DEVICE_ALLOCATION_FLAGS_VASPACE_BIG_PAGE_SIZE_64k 0x00000200u
#define NV_DEVICE_ALLOCATION_VAMODE_OPTIONAL_MULTIPLE_VASPACES 0u
#define NV_CTXSHARE_ALLOCATION_FLAGS_SUBCONTEXT_SYNC 0u
#define NV_CHANNELGPFIFO_NOTIFICATION_TYPE_ERROR 0u
#define NV_CHANNELGPFIFO_NOTIFICATION_TYPE__SIZE_1 3u
#define NVB0B5_ALLOCATION_PARAMETERS_VERSION_1 1u /* clb0b5sw.h: engineType is an NV2080_ENGINE_TYPE */

#define NV2080_CTRL_CMD_GPU_GET_ENGINES_V2 0x20800170u
#define NV2080_GPU_MAX_ENGINES_LIST_SIZE 0x54u
#define NV2080_CTRL_CMD_CE_GET_CAPS_V2 0x20802a03u
#define NV2080_CTRL_CE_CAPS_CE_GRCE 0x01u          /* byte 0 */
#define NV2080_CTRL_CE_CAPS_CE_SHARED 0x02u        /* byte 0 */
#define NV2080_CTRL_CE_CAPS_CE_SYSMEM_WRITE 0x08u  /* byte 0 */
#define NV2080_CTRL_CMD_FB_GET_SEMAPHORE_SURFACE_LAYOUT 0x20801352u
#define NVA06C_CTRL_CMD_GPFIFO_SCHEDULE 0xa06c0101u
#define NVA06F_CTRL_CMD_BIND 0xa06f0104u
#define NVC36F_CTRL_CMD_GPFIFO_GET_WORK_SUBMIT_TOKEN 0xc36f0108u
#define NVC36F_CTRL_CMD_GPFIFO_SET_WORK_SUBMIT_TOKEN_NOTIF_INDEX 0xc36f010au
#define NV_SEMAPHORE_SURFACE_CTRL_CMD_BIND_CHANNEL 0x00da0002u
#define NV_SEMAPHORE_SURFACE_CTRL_CMD_SET_VALUE 0x00da0004u
#define NV_SEMAPHORE_SURFACE_CTRL_CMD_UNBIND_CHANNEL 0x00da0006u

#define NVOS46_FLAGS_CACHE_SNOOP_ENABLE (1u << 4)  /* nvos.h 4:4 */
#define NVOS46_FLAGS_PAGE_SIZE_4KB (1u << 8)       /* nvos.h 11:8 */
#define SYSMEM_MAP_FLAGS (NVOS46_FLAGS_CACHE_SNOOP_ENABLE | NVOS46_FLAGS_PAGE_SIZE_4KB)

typedef struct {
    uint32_t hObjectError, hObjectEccError, hVASpace, engineType;
    uint8_t bIsCallingContextVgpuPlugin;
} NV_CHANNEL_GROUP_ALLOCATION_PARAMETERS;
typedef struct {
    uint32_t hVASpace, flags, subctxId;
} NV_CTXSHARE_ALLOCATION_PARAMETERS;
typedef struct {
    NV_A8 uint64_t base;
    NV_A8 uint64_t size;
    uint32_t addressSpace, cacheAttrib;
} NV_MEMORY_DESC_PARAMS;
typedef struct {
    uint32_t hObjectError, hObjectBuffer;
    NV_A8 uint64_t gpFifoOffset;
    uint32_t gpFifoEntries, flags, hContextShare, hVASpace, hHandleVASpace;
    uint32_t hUserdMemory[8];
    NV_A8 uint64_t userdOffset[8];
    uint32_t engineType, cid, subDeviceId, hObjectEccError;
    NV_MEMORY_DESC_PARAMS instanceMem, userdMem, ramfcMem, mthdbufMem;
    uint32_t hPhysChannelGroup, internalFlags;
    NV_MEMORY_DESC_PARAMS errorNotifierMem, eccErrorNotifierMem;
    uint32_t ProcessID, SubProcessID;
    uint32_t encryptIv[3], decryptIv[3], hmacNonce[8];
    uint32_t tpcConfigID;
} NV_CHANNEL_ALLOC_PARAMS;
typedef struct {
    uint32_t version, engineType;
} NVB0B5_ALLOCATION_PARAMETERS;
typedef struct {
    uint8_t bBar1Mapping, bPriv;
} NV_HOPPER_USERMODE_A_PARAMS;
typedef struct {
    uint8_t bEnable, bSkipSubmit, bSkipEnable;
} NVA06C_CTRL_GPFIFO_SCHEDULE_PARAMS;
typedef struct {
    uint32_t engineCount;
    uint32_t engineList[NV2080_GPU_MAX_ENGINES_LIST_SIZE];
} NV2080_CTRL_GPU_GET_ENGINES_V2_PARAMS;
typedef struct {
    uint32_t ceEngineType;
    uint8_t capsTbl[2];
} NV2080_CTRL_CE_GET_CAPS_V2_PARAMS;
typedef struct {
    uint32_t hChannel, numNotifyIndices, notifyIndices[8];
} NV_SEMAPHORE_SURFACE_CTRL_BIND_CHANNEL_PARAMS;
struct semsurf_alloc {
    uint32_t h_semaphore_mem, h_max_submitted_mem;
    NV_A8 uint64_t flags;
};
struct semsurf_set_value {
    NV_A8 uint64_t index;
    NV_A8 uint64_t new_value;
};
struct semsurf_layout {
    NV_A8 uint64_t max_submitted_offset;
    NV_A8 uint64_t monitored_fence_threshold_offset;
    NV_A8 uint64_t size;
    uint32_t caps, pad;
};
typedef struct { /* nvgputypes.h NvNotification */
    uint32_t timestamp[2];
    uint32_t info32;
    uint16_t info16, status;
} nv_notification;

_Static_assert(sizeof(NV_CHANNEL_GROUP_ALLOCATION_PARAMETERS) == 20, "0xa06c params");
_Static_assert(sizeof(NV_CTXSHARE_ALLOCATION_PARAMETERS) == 12, "0x9067 params");
_Static_assert(sizeof(NV_CHANNEL_ALLOC_PARAMS) == 376, "*6F params");
_Static_assert(sizeof(NVB0B5_ALLOCATION_PARAMETERS) == 8, "*B5 params");
_Static_assert(sizeof(NV_HOPPER_USERMODE_A_PARAMS) == 2, "0xc761 params");
_Static_assert(sizeof(NVA06C_CTRL_GPFIFO_SCHEDULE_PARAMS) == 3, "GPFIFO_SCHEDULE");
_Static_assert(sizeof(NV2080_CTRL_GPU_GET_ENGINES_V2_PARAMS) == 340, "GET_ENGINES_V2");
_Static_assert(sizeof(NV2080_CTRL_CE_GET_CAPS_V2_PARAMS) == 8, "CE_GET_CAPS_V2");
_Static_assert(sizeof(NV_SEMAPHORE_SURFACE_CTRL_BIND_CHANNEL_PARAMS) == 40, "BIND_CHANNEL");
_Static_assert(sizeof(struct semsurf_alloc) == 16, "0xda params");
_Static_assert(sizeof(struct semsurf_layout) == 32, "SEMAPHORE_SURFACE_LAYOUT");
_Static_assert(sizeof(nv_notification) == 16, "NvNotification");

/* ---- Methods ---- */

/* Host (*6F) methods, clc56f.h */
#define HOST_SET_OBJECT 0x0000u
#define HOST_NON_STALL_INTERRUPT 0x0020u
#define HOST_SEM_ADDR_LO 0x005cu
#define SEM_EXECUTE_OPERATION_RELEASE 0x1u
#define SEM_EXECUTE_OPERATION_ACQ_STRICT_GEQ 0x2u
#define SEM_EXECUTE_ACQUIRE_SWITCH_TSG_EN (1u << 12)
#define SEM_EXECUTE_RELEASE_WFI_EN (1u << 20)
#define SEM_EXECUTE_PAYLOAD_SIZE_64BIT (1u << 24)

/* DMA copy (*B5) methods, clc7b5.h / clcab5.h (see the header comment) */
#define NVB5_SET_SEMAPHORE_A 0x0240u
#define NVB5_LAUNCH_DMA 0x0300u
#define NVB5_OFFSET_IN_UPPER 0x0400u
#define NVB5_LAUNCH_DMA_DATA_TRANSFER_TYPE_NONE 0x0u
#define NVB5_LAUNCH_DMA_DATA_TRANSFER_TYPE_NON_PIPELINED 0x2u
#define NVB5_LAUNCH_DMA_FLUSH_ENABLE_TRUE (1u << 2)
#define NVB5_LAUNCH_DMA_SEMAPHORE_TYPE_RELEASE_SEMAPHORE_WITH_TIMESTAMP (2u << 3)
#define NVB5_LAUNCH_DMA_SRC_MEMORY_LAYOUT_PITCH (1u << 7)
#define NVB5_LAUNCH_DMA_DST_MEMORY_LAYOUT_PITCH (1u << 8)
#define NVB5_LAUNCH_DMA_MULTI_LINE_ENABLE_TRUE (1u << 9)
/* SRC_TYPE/DST_TYPE VIRTUAL, FLUSH_TYPE SYS and SEMAPHORE_PAYLOAD_SIZE
 * ONE_WORD are all 0. */

#define SUBC_HOST 0u /* host methods ignore the subchannel (nvk-rm 0006) */
#define SUBC_CE 4u   /* NVK's copy subchannel */

/* USERD (Nvc36fControl .. Nvca6fControl, nvk-rm patch 0002) and doorbell */
#define USERD_GP_GET 0x88u
#define USERD_GP_PUT 0x8cu
#define USERMODE_NOTIFY_CHANNEL_PENDING 0x90u /* clc361.h */
#define USERMODE_SIZE 0x10000u

/* ---- Layout of what the copier owns ---- */

#define GPFIFO_ENTRIES 128u
#define PUSH_SLOT_B 512u
#define PUSH_SLOT_DW (PUSH_SLOT_B / 4u)
#define RING_PUSH_OFFSET 4096u
#define RING_SIZE (128u * 1024u)   /* GPFIFO + 128 push slots of 512 B */
#define CTL_SIZE 8192u             /* error notifier at 0, USERD at 4096 */
#define CTL_USERD_OFFSET 4096u
#define PAGE_4K 4096u
#define STAMP_SIZE 65536u
/* offsets in the stamp page (OS descriptor): 16-byte releases */
#define ST_T0 0u      /* CE semaphore with timestamp before the copy */
#define ST_T1 16u     /* the copy's own semaphore with timestamp */
#define ST_PROBE 32u  /* probe release (64-bit) */
#define ST_K 64u      /* contender completion (64-bit) */
#define ST_K_T0 96u
#define ST_K_T1 112u

#define MIB (1024ull * 1024ull)
#define VA_BASE 0x2000000000ull /* 128 GiB: below 2^40 (GP_ENTRY1_GET_HI is 8 bits) */

/* ---- Small helpers ---- */

static int failed;
static char fail_what[256];
static int fail_status;

static int step(const char *what, int r)
{
    if (r == 0) {
        printf("[ ok ] %s\n", what);
    } else {
        printf("[FAIL] %s: %s (%d / 0x%x) %s\n", what, crm_status_name(r), r, (unsigned)r,
               crm_status_string(r));
        if (!failed) {
            snprintf(fail_what, sizeof fail_what, "%s", what);
            fail_status = r;
        }
        failed = 1;
    }
    fflush(stdout);
    return r;
}

#ifdef _WIN32
static LARGE_INTEGER qpf;
static double now_us(void)
{
    LARGE_INTEGER t;
    QueryPerformanceCounter(&t);
    return (double)t.QuadPart * 1e6 / (double)qpf.QuadPart;
}
static void sleep_ms(unsigned ms) { Sleep(ms); }
static void timer_init(void) { QueryPerformanceFrequency(&qpf); }
#else
static double now_us(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec * 1e6 + (double)ts.tv_nsec / 1e3;
}
static void sleep_ms(unsigned ms)
{
    struct timespec ts = { ms / 1000u, (long)(ms % 1000u) * 1000000L };
    nanosleep(&ts, NULL);
}
static void timer_init(void) {}
#endif

static inline void full_fence(void) { __atomic_thread_fence(__ATOMIC_SEQ_CST); }

static uint64_t align_up(uint64_t v, uint64_t a) { return (v + a - 1) / a * a; }

static uint32_t pattern(size_t i) { return (uint32_t)(i * 2654435761u) ^ 0xc0e5a11du; }

/* Growable sample arrays and statistics */
struct samples {
    const char *name;
    double *v;
    unsigned n, cap;
};

static void add(struct samples *s, double x)
{
    if (s->n == s->cap) {
        unsigned ncap = s->cap ? s->cap * 2 : 256;
        double *nv = realloc(s->v, ncap * sizeof(double));
        if (!nv)
            return;
        s->v = nv;
        s->cap = ncap;
    }
    s->v[s->n++] = x;
}

static int cmp_d(const void *a, const void *b)
{
    const double x = *(const double *)a, y = *(const double *)b;
    return x < y ? -1 : x > y;
}

static void stats_row(struct samples *s)
{
    if (s->n == 0) {
        printf("%-38s %6u %10s %10s %10s %10s %10s\n", s->name, 0u, "-", "-", "-", "-", "-");
        return;
    }
    qsort(s->v, s->n, sizeof(double), cmp_d);
    double sum = 0;
    for (unsigned i = 0; i < s->n; i++)
        sum += s->v[i];
    unsigned p99 = (unsigned)(((uint64_t)s->n * 99) / 100);
    if (p99 >= s->n)
        p99 = s->n - 1;
    printf("%-38s %6u %10.1f %10.1f %10.1f %10.1f %10.1f\n", s->name, s->n, s->v[0], sum / s->n,
           s->v[s->n / 2], s->v[p99], s->v[s->n - 1]);
}

/* ---- GPU memory the copier maps ---- */

struct gmem {
    uint32_t h;        /* RM memory object (own or duplicated) */
    uint64_t size;     /* mapped length */
    uint64_t va;       /* GPU VA in the copier's VA space (0 = unmapped) */
    void *pages;       /* crm_alloc_pages backing (OS descriptor), or NULL */
    uint64_t pages_size;
    void *cpu;         /* CPU mapping through crm_map_memory, or NULL */
};

static uint64_t next_va = VA_BASE;

/* GPU-map at a fixed, 2 MiB aligned VA below 2^40 (RM aligns a default page
 * size virtual allocation to 64 KiB, and offset and size to 2 MiB from 2 MiB
 * up: nvk-rm patch 0008); RM's choice if the fixed range is refused. */
static int gpu_map(crm_client *c, uint32_t dev, uint32_t vas, struct gmem *m, uint32_t flags,
                   const char *what)
{
    char w[200];
    uint64_t va = next_va;
    int r = crm_map_dma2(c, dev, vas, m->h, 0, m->size, flags, 0, 0, &va);
    if (r > 0) {
        va = 0;
        r = crm_map_dma2(c, dev, vas, m->h, 0, m->size, flags, 0, 0, &va);
    }
    snprintf(w, sizeof w, "GPU-map %s (%" PRIu64 " bytes) -> VA 0x%" PRIx64, what, m->size, va);
    if (step(w, r))
        return r;
    m->va = va;
    if (va == next_va)
        next_va += align_up(m->size, 2 * MIB) + 2 * MIB;
    return 0;
}

static void gpu_unmap(crm_client *c, uint32_t dev, uint32_t vas, struct gmem *m)
{
    if (m->va)
        crm_unmap_dma(c, dev, vas, m->h, 0, m->va);
    m->va = 0;
}

/* OS descriptor over process pages, GPU-mapped snooped (as the KMD's lease
 * pages would be) */
static int osdesc_alloc(crm_client *c, uint32_t dev, uint32_t vas, struct gmem *m, uint64_t size,
                        const char *what)
{
    char w[200];
    memset(m, 0, sizeof *m);
    m->pages_size = align_up(size, 64 * 1024);
    snprintf(w, sizeof w, "crm_alloc_pages %" PRIu64 " bytes (%s)", m->pages_size, what);
    if (step(w, crm_alloc_pages(c, m->pages_size, &m->pages))) {
        m->pages = NULL;
        return -1;
    }
    memset(m->pages, 0, m->pages_size);
    snprintf(w, sizeof w, "alloc NV01_MEMORY_SYSTEM_OS_DESCRIPTOR 0x71 (%s, %" PRIu64 " pages)", what,
             m->pages_size / PAGE_4K);
    if (step(w, crm_alloc_os_descriptor(c, dev, &m->h, m->pages, m->pages_size, 0))) {
        m->h = 0;
        return -1;
    }
    m->size = m->pages_size;
    m->cpu = m->pages;
    return gpu_map(c, dev, vas, m, SYSMEM_MAP_FLAGS, what);
}

static void osdesc_free(crm_client *c, uint32_t dev, uint32_t vas, struct gmem *m)
{
    gpu_unmap(c, dev, vas, m);
    if (m->h)
        crm_free(c, dev, m->h);
    if (m->pages)
        crm_free_pages(c, m->pages, m->pages_size);
    memset(m, 0, sizeof *m);
}

static int sysmem_alloc(crm_client *c, uint32_t dev, uint32_t *h, uint64_t size, const char *what)
{
    char w[200];
    NV_MEMORY_ALLOCATION_PARAMS mp;
    memset(&mp, 0, sizeof mp);
    mp.owner = crm_root(c);
    mp.type = NVOS32_TYPE_IMAGE;
    mp.attr = (NVOS32_ATTR_LOCATION_PCI << NVOS32_ATTR_LOCATION_SHIFT) |
              (NVOS32_ATTR_PAGE_SIZE_4KB << NVOS32_ATTR_PAGE_SIZE_SHIFT) |
              (NVOS32_ATTR_PHYSICALITY_NONCONTIGUOUS << NVOS32_ATTR_PHYSICALITY_SHIFT) |
              (NVOS32_ATTR_COHERENCY_CACHED << NVOS32_ATTR_COHERENCY_SHIFT);
    mp.size = size;
    snprintf(w, sizeof w, "alloc NV01_MEMORY_SYSTEM 0x3e %" PRIu64 " bytes (%s)", size, what);
    return step(w, crm_alloc(c, dev, h, NV01_MEMORY_SYSTEM, &mp, sizeof mp));
}

static int vidmem_alloc(crm_client *c, uint32_t dev, uint32_t *h, uint64_t size, const char *what)
{
    char w[200];
    NV_MEMORY_ALLOCATION_PARAMS mp;
    memset(&mp, 0, sizeof mp);
    mp.owner = crm_root(c);
    mp.type = NVOS32_TYPE_IMAGE;
    mp.flags = NVOS32_ALLOC_FLAGS_ALIGNMENT_FORCE | NVOS32_ALLOC_FLAGS_IGNORE_BANK_PLACEMENT;
    mp.attr = (NVOS32_ATTR_LOCATION_VIDMEM << NVOS32_ATTR_LOCATION_SHIFT) |
              (NVOS32_ATTR_PAGE_SIZE_HUGE << NVOS32_ATTR_PAGE_SIZE_SHIFT) |
              (NVOS32_ATTR_PHYSICALITY_CONTIGUOUS << NVOS32_ATTR_PHYSICALITY_SHIFT);
    mp.size = size;
    mp.alignment = 2 * MIB;
    snprintf(w, sizeof w, "alloc NV01_MEMORY_LOCAL_USER 0x40 %" PRIu64 " bytes (%s)", size, what);
    return step(w, crm_alloc(c, dev, h, NV01_MEMORY_LOCAL_USER, &mp, sizeof mp));
}

/* ---- Channels ---- */

struct gen {
    const char *name;
    uint32_t gpfifo, ce, usermode;
    const char *gpfifo_name, *ce_name;
};
static const struct gen gens[] = {
    { "gb202", 0xca6f, 0xcab5, 0xc761, "BLACKWELL_CHANNEL_GPFIFO_B", "BLACKWELL_DMA_COPY_B" },
    { "ada", 0xc56f, 0xc7b5, 0xc561, "AMPERE_CHANNEL_GPFIFO_A", "AMPERE_DMA_COPY_B" },
};

struct copier {
    crm_client *c;
    uint32_t dev, sub, vas, um;
    void *um_map;
    const struct gen *g;
};

struct chan {
    const char *label;
    int gr;                 /* graphics engine's CE */
    uint32_t engine_type;
    uint32_t tsg, ctxshare, ch, ce;
    struct gmem ring;       /* GPFIFO + push slots, OS descriptor */
    uint32_t ctl;           /* RM sysmem: error notifier + USERD */
    void *ctl_map;
    volatile nv_notification *notifier;
    volatile uint32_t *userd;
    uint32_t token, put, gp_put_last;
    int scheduled;
};

static uint32_t mthd(uint32_t subc, uint32_t m, uint32_t count)
{
    /* SEC_OP = INC_METHOD (1 << 29), nvk-rm patch 0006 host_mthd_incr */
    return (1u << 29) | (count << 16) | (subc << 13) | (m >> 2);
}

static const char *engine_name(uint32_t t, char *buf, size_t n)
{
    if (t == NV2080_ENGINE_TYPE_GRAPHICS)
        snprintf(buf, n, "GR");
    else if (t >= NV2080_ENGINE_TYPE_COPY0 && t <= NV2080_ENGINE_TYPE_COPY9)
        snprintf(buf, n, "COPY%u", t - NV2080_ENGINE_TYPE_COPY0);
    else if (t >= NV2080_ENGINE_TYPE_COPY10 && t <= NV2080_ENGINE_TYPE_COPY19)
        snprintf(buf, n, "COPY%u", t - NV2080_ENGINE_TYPE_COPY10 + 10);
    else
        snprintf(buf, n, "engine 0x%x", t);
    return buf;
}

static void chan_destroy(struct copier *k, struct chan *ch)
{
    if (ch->tsg) {
        if (ch->scheduled) {
            NVA06C_CTRL_GPFIFO_SCHEDULE_PARAMS sp = { .bEnable = 0 };
            crm_control(k->c, ch->tsg, NVA06C_CTRL_CMD_GPFIFO_SCHEDULE, &sp, sizeof sp);
        }
        /* Freeing the TSG frees the subcontext, channel and CE object */
        char w[160];
        snprintf(w, sizeof w, "%s: free the channel group (channel, CE object)", ch->label);
        step(w, crm_free(k->c, k->dev, ch->tsg));
    }
    if (ch->ctl_map)
        crm_unmap_memory(k->c, k->dev, ch->ctl, ch->ctl_map, CTL_SIZE, 0);
    if (ch->ctl)
        crm_free(k->c, k->dev, ch->ctl);
    osdesc_free(k->c, k->dev, k->vas, &ch->ring);
    memset(ch, 0, sizeof *ch);
}

/* Section 1.1 of the design, with the engine of patch 0035 */
static int chan_create(struct copier *k, struct chan *ch, const char *label, int gr,
                       uint32_t engine_type)
{
    char w[256], en[32];
    memset(ch, 0, sizeof *ch);
    ch->label = label;
    ch->gr = gr;
    ch->engine_type = gr ? NV2080_ENGINE_TYPE_GRAPHICS : engine_type;
    engine_name(ch->engine_type, en, sizeof en);

    snprintf(w, sizeof w, "%s ring", label);
    if (osdesc_alloc(k->c, k->dev, k->vas, &ch->ring, RING_SIZE, w))
        return -1;
    if (ch->ring.va + RING_SIZE > (1ull << 40)) {
        printf("[FAIL] %s: ring VA 0x%" PRIx64 " is not below 2^40\n", label, ch->ring.va);
        step("ring VA below 2^40", -ERANGE);
        return -1;
    }
    snprintf(w, sizeof w, "%s error notifier + USERD", label);
    if (sysmem_alloc(k->c, k->dev, &ch->ctl, CTL_SIZE, w))
        return -1;
    if (step("CPU-map error notifier + USERD", crm_map_memory(k->c, k->dev, ch->ctl, 0, CTL_SIZE, 0,
                                                             &ch->ctl_map))) {
        ch->ctl_map = NULL;
        return -1;
    }
    memset(ch->ctl_map, 0, CTL_SIZE);
    ch->notifier = ch->ctl_map;
    ch->userd = (volatile uint32_t *)((uint8_t *)ch->ctl_map + CTL_USERD_OFFSET);

    NV_CHANNEL_GROUP_ALLOCATION_PARAMETERS tp;
    memset(&tp, 0, sizeof tp);
    tp.hVASpace = k->vas;
    tp.engineType = ch->engine_type;
    snprintf(w, sizeof w, "%s: alloc KEPLER_CHANNEL_GROUP_A 0xa06c {engineType %s 0x%x}", label, en,
             ch->engine_type);
    if (step(w, crm_alloc(k->c, k->dev, &ch->tsg, KEPLER_CHANNEL_GROUP_A, &tp, sizeof tp))) {
        ch->tsg = 0;
        return -1;
    }
    NV_CTXSHARE_ALLOCATION_PARAMETERS cp = { .hVASpace = k->vas,
                                             .flags = NV_CTXSHARE_ALLOCATION_FLAGS_SUBCONTEXT_SYNC };
    snprintf(w, sizeof w, "%s: alloc FERMI_CONTEXT_SHARE_A 0x9067 (SYNC)", label);
    if (step(w, crm_alloc(k->c, ch->tsg, &ch->ctxshare, FERMI_CONTEXT_SHARE_A, &cp, sizeof cp)))
        return -1;

    NV_CHANNEL_ALLOC_PARAMS chp;
    memset(&chp, 0, sizeof chp);
    chp.hObjectError = ch->ctl;
    chp.gpFifoOffset = ch->ring.va;
    chp.gpFifoEntries = GPFIFO_ENTRIES;
    chp.hContextShare = ch->ctxshare;
    chp.hUserdMemory[0] = ch->ctl;
    chp.userdOffset[0] = CTL_USERD_OFFSET;
    chp.engineType = ch->engine_type;
    snprintf(w, sizeof w, "%s: alloc %s 0x%x {engineType %s, %u entries}", label,
             k->g->gpfifo_name, k->g->gpfifo, en, GPFIFO_ENTRIES);
    if (step(w, crm_alloc(k->c, ch->tsg, &ch->ch, k->g->gpfifo, &chp, sizeof chp)))
        return -1;

    uint32_t bind = ch->engine_type;
    snprintf(w, sizeof w, "%s: NVA06F_CTRL_CMD_BIND 0xa06f0104 {%s}", label, en);
    if (step(w, crm_control(k->c, ch->ch, NVA06F_CTRL_CMD_BIND, &bind, sizeof bind)))
        return -1;

    if (gr) {
        /* As NVK: NULL parameters, RM picks the GR copy engine (patch 0006) */
        snprintf(w, sizeof w, "%s: alloc %s 0x%x (NULL params: the graphics CE)", label,
                 k->g->ce_name, k->g->ce);
        if (step(w, crm_alloc(k->c, ch->ch, &ch->ce, k->g->ce, NULL, 0)))
            return -1;
    } else {
        NVB0B5_ALLOCATION_PARAMETERS bp = { .version = NVB0B5_ALLOCATION_PARAMETERS_VERSION_1,
                                            .engineType = ch->engine_type };
        snprintf(w, sizeof w, "%s: alloc %s 0x%x {VERSION_1, %s}", label, k->g->ce_name, k->g->ce,
                 en);
        if (step(w, crm_alloc(k->c, ch->ch, &ch->ce, k->g->ce, &bp, sizeof bp)))
            return -1;
    }

    uint32_t idx = NV_CHANNELGPFIFO_NOTIFICATION_TYPE__SIZE_1;
    snprintf(w, sizeof w, "%s: SET_WORK_SUBMIT_TOKEN_NOTIF_INDEX 0xc36f010a", label);
    if (step(w, crm_control(k->c, ch->ch, NVC36F_CTRL_CMD_GPFIFO_SET_WORK_SUBMIT_TOKEN_NOTIF_INDEX,
                            &idx, sizeof idx)))
        return -1;
    uint32_t tok = 0;
    snprintf(w, sizeof w, "%s: GET_WORK_SUBMIT_TOKEN 0xc36f0108", label);
    if (step(w, crm_control(k->c, ch->ch, NVC36F_CTRL_CMD_GPFIFO_GET_WORK_SUBMIT_TOKEN, &tok,
                            sizeof tok)))
        return -1;
    ch->token = tok;
    NVA06C_CTRL_GPFIFO_SCHEDULE_PARAMS sp = { .bEnable = 1 };
    snprintf(w, sizeof w, "%s: NVA06C_CTRL_CMD_GPFIFO_SCHEDULE 0xa06c0101 {enable}", label);
    if (step(w, crm_control(k->c, ch->tsg, NVA06C_CTRL_CMD_GPFIFO_SCHEDULE, &sp, sizeof sp)))
        return -1;
    ch->scheduled = 1;
    printf("       %s: engine %s (type 0x%x), token 0x%08x -> runlist %u, channel id %u\n", label, en,
           ch->engine_type, tok, (tok >> 16) & 0x7fu, tok & 0xfffu);
    return 0;
}

/* One push slot per GPFIFO entry; every round waits for its push, so a slot
 * is never reused while the GPU may still read it. */
static uint32_t *slot(struct chan *ch)
{
    return (uint32_t *)((uint8_t *)ch->ring.cpu + RING_PUSH_OFFSET) + ch->put * PUSH_SLOT_DW;
}

static void kick(struct copier *k, struct chan *ch, unsigned dw)
{
    const uint64_t addr = ch->ring.va + RING_PUSH_OFFSET + (uint64_t)ch->put * PUSH_SLOT_B;
    /* GP_ENTRY0 GET 31:2, GP_ENTRY1 GET_HI 7:0, LENGTH 30:10 (patch 0002) */
    volatile uint64_t *gp = (volatile uint64_t *)ch->ring.cpu;
    gp[ch->put] = ((uint64_t)(((uint32_t)(addr >> 32) & 0xffu) | ((dw & 0x1fffffu) << 10)) << 32) |
                  ((uint32_t)addr & ~3u);
    ch->put = (ch->put + 1) % GPFIFO_ENTRIES;
    full_fence();
    ch->userd[USERD_GP_PUT / 4] = ch->put;
    ch->gp_put_last = ch->put;
    full_fence();
    *(volatile uint32_t *)((uint8_t *)k->um_map + USERMODE_NOTIFY_CHANNEL_PENDING) = ch->token;
}

static unsigned emit_host_sem(uint32_t *p, unsigned n, uint64_t va, uint64_t value, uint32_t exec)
{
    p[n++] = mthd(SUBC_HOST, HOST_SEM_ADDR_LO, 5);
    p[n++] = (uint32_t)va;
    p[n++] = (uint32_t)(va >> 32);
    p[n++] = (uint32_t)value;
    p[n++] = (uint32_t)(value >> 32);
    p[n++] = exec;
    return n;
}

static unsigned emit_ce_sem_addr(uint32_t *p, unsigned n, uint64_t va, uint32_t payload)
{
    p[n++] = mthd(SUBC_CE, NVB5_SET_SEMAPHORE_A, 3);
    p[n++] = (uint32_t)(va >> 32) & 0x1ffffffu; /* SET_SEMAPHORE_A_UPPER 24:0 */
    p[n++] = (uint32_t)va;
    p[n++] = payload;
    return n;
}

/* T0 (CE semaphore-only launch with timestamp), the pitch copy with its own
 * timestamped semaphore (T1) */
static unsigned emit_ce_copy(uint32_t *p, unsigned n, uint64_t src, uint64_t dst, uint32_t pitch,
                             uint32_t lines, uint64_t t0_va, uint64_t t1_va, uint32_t payload)
{
    n = emit_ce_sem_addr(p, n, t0_va, payload);
    p[n++] = mthd(SUBC_CE, NVB5_LAUNCH_DMA, 1);
    p[n++] = NVB5_LAUNCH_DMA_DATA_TRANSFER_TYPE_NONE | NVB5_LAUNCH_DMA_FLUSH_ENABLE_TRUE |
             NVB5_LAUNCH_DMA_SEMAPHORE_TYPE_RELEASE_SEMAPHORE_WITH_TIMESTAMP;
    n = emit_ce_sem_addr(p, n, t1_va, payload);
    p[n++] = mthd(SUBC_CE, NVB5_OFFSET_IN_UPPER, 8);
    p[n++] = (uint32_t)(src >> 32) & 0x1ffffffu;
    p[n++] = (uint32_t)src;
    p[n++] = (uint32_t)(dst >> 32) & 0x1ffffffu;
    p[n++] = (uint32_t)dst;
    p[n++] = pitch; /* PITCH_IN */
    p[n++] = pitch; /* PITCH_OUT */
    p[n++] = pitch; /* LINE_LENGTH_IN (bytes) */
    p[n++] = lines; /* LINE_COUNT */
    p[n++] = mthd(SUBC_CE, NVB5_LAUNCH_DMA, 1);
    p[n++] = NVB5_LAUNCH_DMA_DATA_TRANSFER_TYPE_NON_PIPELINED | NVB5_LAUNCH_DMA_FLUSH_ENABLE_TRUE |
             NVB5_LAUNCH_DMA_SEMAPHORE_TYPE_RELEASE_SEMAPHORE_WITH_TIMESTAMP |
             NVB5_LAUNCH_DMA_SRC_MEMORY_LAYOUT_PITCH | NVB5_LAUNCH_DMA_DST_MEMORY_LAYOUT_PITCH |
             NVB5_LAUNCH_DMA_MULTI_LINE_ENABLE_TRUE;
    return n;
}

static unsigned emit_release(uint32_t *p, unsigned n, uint64_t va, uint64_t value, int wfi, int irq)
{
    n = emit_host_sem(p, n, va, value,
                      SEM_EXECUTE_OPERATION_RELEASE | SEM_EXECUTE_PAYLOAD_SIZE_64BIT |
                          (wfi ? SEM_EXECUTE_RELEASE_WFI_EN : 0));
    if (irq) {
        p[n++] = mthd(SUBC_HOST, HOST_NON_STALL_INTERRUPT, 1);
        p[n++] = 0;
    }
    return n;
}

/* Spin until *v >= want or the deadline; 1 when reached */
static int spin_ge(volatile uint64_t *v, uint64_t want, double deadline_us)
{
    while (*v < want) {
        if (now_us() > deadline_us)
            return 0;
    }
    return 1;
}
static int spin_ge32(volatile uint32_t *v, uint32_t want, double deadline_us)
{
    while ((int32_t)(*v - want) < 0) {
        if (now_us() > deadline_us)
            return 0;
    }
    return 1;
}

static int channel_error(struct chan *ch)
{
    const uint16_t st = ch->notifier[NV_CHANNELGPFIFO_NOTIFICATION_TYPE_ERROR].status;
    if (st == 0)
        return 0;
    printf("[FAIL] %s: channel error notifier status 0x%x info32 0x%x (RC error)\n", ch->label, st,
           ch->notifier[NV_CHANNELGPFIFO_NOTIFICATION_TYPE_ERROR].info32);
    return st;
}

/* ---- Options ---- */

struct opts {
    const struct gen *g;
    int engine;         /* -1 auto */
    int engine_gr;
    unsigned iterations;
    double duration_s;
    double delay_s;
    unsigned hold_ms;
    uint32_t width, height;
    int src_sys;
    int contend;
    int contend_engine; /* -1 = gr */
    unsigned contend_mb;
    int new_vas;
    int release_semsurf;
    unsigned timeout_ms;
    int fence;
};

static int parse(int argc, char **argv, struct opts *o)
{
    memset(o, 0, sizeof *o);
    o->g = &gens[0];
    o->engine = -1;
    o->iterations = 200;
    o->hold_ms = 2;
    o->width = 1600;
    o->height = 900;
    o->contend_engine = -1;
    o->contend_mb = 64;
    o->timeout_ms = 2000;
    for (int i = 1; i < argc; i++) {
        const char *a = argv[i];
        const char *v = i + 1 < argc ? argv[i + 1] : NULL;
#define NEEDV                                                \
    do {                                                     \
        if (!v) {                                            \
            fprintf(stderr, "%s needs a value\n", a);        \
            return -1;                                       \
        }                                                    \
        i++;                                                 \
    } while (0)
        if (!strcmp(a, "--gen")) {
            NEEDV;
            if (!strcmp(v, "gb202") || !strcmp(v, "blackwell"))
                o->g = &gens[0];
            else if (!strcmp(v, "ada") || !strcmp(v, "ad10x") || !strcmp(v, "ampere"))
                o->g = &gens[1];
            else {
                fprintf(stderr, "--gen gb202|ada\n");
                return -1;
            }
        } else if (!strcmp(a, "--engine")) {
            NEEDV;
            if (!strcmp(v, "gr"))
                o->engine_gr = 1;
            else
                o->engine = atoi(v);
        } else if (!strcmp(a, "--iterations")) {
            NEEDV;
            o->iterations = (unsigned)strtoul(v, NULL, 0);
        } else if (!strcmp(a, "--duration")) {
            NEEDV;
            o->duration_s = atof(v);
        } else if (!strcmp(a, "--delay")) {
            NEEDV;
            o->delay_s = atof(v);
        } else if (!strcmp(a, "--hold-ms")) {
            NEEDV;
            o->hold_ms = (unsigned)strtoul(v, NULL, 0);
        } else if (!strcmp(a, "--size")) {
            NEEDV;
            if (sscanf(v, "%ux%u", &o->width, &o->height) != 2 || !o->width || !o->height) {
                fprintf(stderr, "--size WxH\n");
                return -1;
            }
        } else if (!strcmp(a, "--src")) {
            NEEDV;
            o->src_sys = !strcmp(v, "sys");
        } else if (!strcmp(a, "--contend")) {
            o->contend = 1;
        } else if (!strcmp(a, "--contend-engine")) {
            NEEDV;
            o->contend_engine = !strcmp(v, "gr") ? -1 : atoi(v);
        } else if (!strcmp(a, "--contend-mb")) {
            NEEDV;
            o->contend_mb = (unsigned)strtoul(v, NULL, 0);
        } else if (!strcmp(a, "--vas")) {
            NEEDV;
            o->new_vas = !strcmp(v, "new");
        } else if (!strcmp(a, "--release")) {
            NEEDV;
            o->release_semsurf = !strcmp(v, "semsurf");
        } else if (!strcmp(a, "--timeout-ms")) {
            NEEDV;
            o->timeout_ms = (unsigned)strtoul(v, NULL, 0);
        } else if (!strcmp(a, "--fence")) {
            o->fence = 1;
        } else {
            fprintf(stderr, "unknown option %s (see the header of crm_ce_copy_smoke.c)\n", a);
            return -1;
        }
#undef NEEDV
    }
    if (o->iterations == 0)
        o->iterations = 1;
    if (o->timeout_ms < 100)
        o->timeout_ms = 100;
    if (o->contend_mb < 1)
        o->contend_mb = 1;
    if ((uint64_t)o->width * 4u * o->height > 1024ull * MIB) {
        fprintf(stderr, "--size: at most 1 GiB\n");
        return -1;
    }
    return 0;
}

/* ---- main ---- */

int main(int argc, char **argv)
{
    struct opts o;
    if (parse(argc, argv, &o))
        return 1;
    timer_init();

    const uint32_t pitch = o.width * 4u;
    const uint64_t copy_bytes = (uint64_t)pitch * o.height;
    const uint64_t src_size = align_up(copy_bytes, 2 * MIB);
    const uint64_t contend_bytes = (uint64_t)o.contend_mb * MIB;
    printf("crm_ce_copy_smoke: gen %s (%s 0x%x, %s 0x%x, usermode 0x%x), copy %ux%u = %" PRIu64
           " bytes, source %s\n",
           o.g->name, o.g->gpfifo_name, o.g->gpfifo, o.g->ce_name, o.g->ce, o.g->usermode, o.width,
           o.height, copy_bytes, o.src_sys ? "system memory" : "video memory");

    /* Producer */
    crm_client *pc = NULL;
    uint32_t p_dev = 0, p_sub = 0, p_sem = 0, p_semsurf = 0, p_src = 0;
    void *p_sem_map = NULL, *p_src_map = NULL;
    volatile uint64_t *prod = NULL;
    /* Copier */
    struct copier k;
    memset(&k, 0, sizeof k);
    k.g = o.g;
    struct chan ch, cch;
    memset(&ch, 0, sizeof ch);
    memset(&cch, 0, sizeof cch);
    struct gmem d_sem, d_src, dst, stamp, done, csrc, cdst;
    memset(&d_sem, 0, sizeof d_sem);
    memset(&d_src, 0, sizeof d_src);
    memset(&dst, 0, sizeof dst);
    memset(&stamp, 0, sizeof stamp);
    memset(&done, 0, sizeof done);
    memset(&csrc, 0, sizeof csrc);
    memset(&cdst, 0, sizeof cdst);
    uint32_t done_semsurf = 0;
    int semsurf_bound = 0;
#ifdef _WIN32
    int drm_fd = -1;
    uint32_t fence_ctx = 0;
#endif
    int contend_ok = 0;
    char w[256];

    struct samples s_ready = { .name = "submit_to_done_acquire_satisfied_us" };
    struct samples s_wait = { .name = "acquire_satisfy_to_done_us" };
    struct samples s_probe = { .name = "doorbell_to_gpfifo_get_us" };
    struct samples s_copy = { .name = "copy_us" };
    struct samples s_copy_wait = { .name = "copy_us_wait_stage" };
    struct samples s_event = { .name = "doorbell_to_event_us" };
    struct samples s_c_ready = { .name = "submit_to_done_contended_us" };
    struct samples s_c_copy = { .name = "copy_us_contended" };
    struct samples s_c_other = { .name = "contend_copy_us" };
    struct samples s_setval = { .name = "set_value_call_us" };
    unsigned held = 0, wait_rounds = 0, verified = 0, overlapped = 0, contended = 0;
    uint64_t bad_bytes = 0;
    uint32_t gp_get_seen = 0;

    if (step("producer: crm_open", crm_open(&pc, NULL))) {
        pc = NULL;
        goto out;
    }
    printf("       RM version %s, producer client 0x%08x\n", crm_rm_version(pc), crm_root(pc));
    {
        NV0080_ALLOC_PARAMETERS dp;
        memset(&dp, 0, sizeof dp);
        if (step("producer: alloc NV01_DEVICE_0", crm_alloc(pc, crm_root(pc), &p_dev, NV01_DEVICE_0, &dp, sizeof dp)))
            goto out;
        NV2080_ALLOC_PARAMETERS sp = { .subDeviceId = 0 };
        if (step("producer: alloc NV20_SUBDEVICE_0", crm_alloc(pc, p_dev, &p_sub, NV20_SUBDEVICE_0, &sp, sizeof sp)))
            goto out;
    }
    if (sysmem_alloc(pc, p_dev, &p_sem, PAGE_4K, "producer timeline"))
        goto out;
    if (step("producer: CPU-map the timeline", crm_map_memory(pc, p_dev, p_sem, 0, PAGE_4K, 0, &p_sem_map))) {
        p_sem_map = NULL;
        goto out;
    }
    memset(p_sem_map, 0, PAGE_4K);
    prod = p_sem_map;
    {
        struct semsurf_alloc sa = { .h_semaphore_mem = p_sem };
        int r = crm_alloc(pc, p_sub, &p_semsurf, NV_SEMAPHORE_SURFACE, &sa, sizeof sa);
        if (r) {
            printf("[info] producer: NV_SEMAPHORE_SURFACE 0xda over the timeline: %s (0x%x); "
                   "the CPU stores the value\n", crm_status_name(r), (unsigned)r);
            p_semsurf = 0;
            if (o.release_semsurf) {
                step("--release semsurf needs the semaphore surface", r);
                goto out;
            }
        } else {
            printf("[ ok ] producer: NV_SEMAPHORE_SURFACE 0xda over the timeline (slot 0)\n");
        }
    }
    if (!o.src_sys) {
        if (vidmem_alloc(pc, p_dev, &p_src, src_size, "source")) {
            p_src = 0;
            goto out;
        }
        int r = crm_map_memory(pc, p_dev, p_src, 0, src_size, 0, &p_src_map);
        if (r) {
            /* No BAR1 mapping of the whole source: fall back to system memory */
            printf("[info] producer: BAR1 CPU map of the video-memory source: %s (0x%x); using a "
                   "system-memory source\n", crm_status_name(r), (unsigned)r);
            p_src_map = NULL;
            crm_free(pc, p_dev, p_src);
            p_src = 0;
            o.src_sys = 1;
        }
    }
    if (o.src_sys) {
        if (sysmem_alloc(pc, p_dev, &p_src, src_size, "source")) {
            p_src = 0;
            goto out;
        }
        if (step("producer: CPU-map the source", crm_map_memory(pc, p_dev, p_src, 0, src_size, 0, &p_src_map))) {
            p_src_map = NULL;
            goto out;
        }
    }
    {
        volatile uint32_t *sw = p_src_map;
        for (size_t i = 0; i < copy_bytes / 4; i++)
            sw[i] = pattern(i);
        size_t bad = 0;
        for (size_t i = 0; i < copy_bytes / 4; i += 997)
            bad += sw[i] != pattern(i);
        snprintf(w, sizeof w, "producer: source (%s, pitch layout) filled with the pattern",
                 o.src_sys ? "system memory" : "video memory through BAR1");
        if (step(w, bad ? -EIO : 0))
            goto out;
    }

    /* Copier */
    if (step("copier: crm_open", crm_open(&k.c, NULL))) {
        k.c = NULL;
        goto out;
    }
    printf("       copier client 0x%08x\n", crm_root(k.c));
    {
        NV0080_ALLOC_PARAMETERS dp;
        memset(&dp, 0, sizeof dp);
        dp.hClientShare = crm_root(k.c);
        dp.flags = NV_DEVICE_ALLOCATION_FLAGS_VASPACE_BIG_PAGE_SIZE_64k;
        dp.vaMode = NV_DEVICE_ALLOCATION_VAMODE_OPTIONAL_MULTIPLE_VASPACES;
        if (step("copier: alloc NV01_DEVICE_0 (OPTIONAL_MULTIPLE_VASPACES, 64 KiB big pages)",
                 crm_alloc(k.c, crm_root(k.c), &k.dev, NV01_DEVICE_0, &dp, sizeof dp)))
            goto out;
        NV2080_ALLOC_PARAMETERS sp = { .subDeviceId = 0 };
        if (step("copier: alloc NV20_SUBDEVICE_0", crm_alloc(k.c, k.dev, &k.sub, NV20_SUBDEVICE_0, &sp, sizeof sp)))
            goto out;
        NV_VASPACE_ALLOCATION_PARAMETERS vp;
        memset(&vp, 0, sizeof vp);
        vp.index = o.new_vas ? 0 : NV_VASPACE_ALLOCATION_INDEX_GPU_DEVICE;
        snprintf(w, sizeof w, "copier: alloc FERMI_VASPACE_A 0x90f1 (%s)",
                 o.new_vas ? "a new VA space" : "index GPU_DEVICE: the device's VA space");
        if (step(w, crm_alloc(k.c, k.dev, &k.vas, FERMI_VASPACE_A, &vp, sizeof vp)))
            goto out;
    }

    /* Engines: GET_ENGINES_V2, CE_GET_CAPS_V2 per copy engine */
    uint32_t engine_type = 0;
    int first_async = -1, grce_idx = -1;
    {
        NV2080_CTRL_GPU_GET_ENGINES_V2_PARAMS ep;
        memset(&ep, 0, sizeof ep);
        if (step("copier: NV2080_CTRL_CMD_GPU_GET_ENGINES_V2 0x20800170",
                 crm_control(k.c, k.sub, NV2080_CTRL_CMD_GPU_GET_ENGINES_V2, &ep, sizeof ep)))
            goto out;
        if (ep.engineCount > NV2080_GPU_MAX_ENGINES_LIST_SIZE)
            ep.engineCount = NV2080_GPU_MAX_ENGINES_LIST_SIZE;
        printf("       %u engines:", ep.engineCount);
        for (uint32_t i = 0; i < ep.engineCount; i++)
            printf(" 0x%x", ep.engineList[i]);
        printf("\n");
        for (uint32_t i = 0; i < ep.engineCount; i++) {
            const uint32_t t = ep.engineList[i];
            int idx;
            if (t >= NV2080_ENGINE_TYPE_COPY0 && t <= NV2080_ENGINE_TYPE_COPY9)
                idx = (int)(t - NV2080_ENGINE_TYPE_COPY0);
            else if (t >= NV2080_ENGINE_TYPE_COPY10 && t <= NV2080_ENGINE_TYPE_COPY19)
                idx = (int)(t - NV2080_ENGINE_TYPE_COPY10) + 10;
            else
                continue;
            NV2080_CTRL_CE_GET_CAPS_V2_PARAMS cp;
            memset(&cp, 0, sizeof cp);
            cp.ceEngineType = t;
            int r = crm_control(k.c, k.sub, NV2080_CTRL_CMD_CE_GET_CAPS_V2, &cp, sizeof cp);
            if (r) {
                printf("       COPY%d: CE_GET_CAPS_V2 0x20802a03: %s (0x%x)\n", idx, crm_status_name(r),
                       (unsigned)r);
                continue;
            }
            const int is_gr = !!(cp.capsTbl[0] & NV2080_CTRL_CE_CAPS_CE_GRCE);
            printf("       COPY%d (type 0x%x): caps %02x %02x%s%s%s\n", idx, t, cp.capsTbl[0],
                   cp.capsTbl[1], is_gr ? " GRCE" : " async",
                   (cp.capsTbl[0] & NV2080_CTRL_CE_CAPS_CE_SHARED) ? " SHARED" : "",
                   (cp.capsTbl[0] & NV2080_CTRL_CE_CAPS_CE_SYSMEM_WRITE) ? " SYSMEM_WRITE" : "");
            if (is_gr && grce_idx < 0)
                grce_idx = idx;
            if (!is_gr && first_async < 0)
                first_async = idx;
        }
    }
    if (!o.engine_gr) {
        if (o.engine < 0)
            o.engine = first_async;
        if (o.engine < 0) {
            step("an async copy engine (none reported; try --engine gr or --engine <n>)", -ENODEV);
            goto out;
        }
        engine_type = NV2080_ENGINE_TYPE_COPY(o.engine);
    }

    /* Doorbell (crm_smoke.c 164-191: CPU-mapped through the subdevice) */
    {
        NV_HOPPER_USERMODE_A_PARAMS up = { .bBar1Mapping = 1, .bPriv = 0 };
        const int with_params = o.g->usermode >= 0xc661;
        snprintf(w, sizeof w, "copier: alloc usermode 0x%x under the subdevice", o.g->usermode);
        if (step(w, crm_alloc(k.c, k.sub, &k.um, o.g->usermode, with_params ? &up : NULL,
                              with_params ? sizeof up : 0))) {
            k.um = 0;
            goto out;
        }
        if (step("copier: CPU-map the doorbell (64 KiB, via the subdevice)",
                 crm_map_memory(k.c, k.sub, k.um, 0, USERMODE_SIZE, 0, &k.um_map))) {
            k.um_map = NULL;
            goto out;
        }
    }

    if (chan_create(&k, &ch, o.engine_gr ? "ce channel (GR CE)" : "ce channel", o.engine_gr, engine_type))
        goto out;

    /* Cross-client duplicates of the producer's timeline and source */
    snprintf(w, sizeof w, "copier: DUP_OBJECT producer timeline 0x%08x of client 0x%08x", p_sem, crm_root(pc));
    if (step(w, crm_dup_object(k.c, k.dev, &d_sem.h, crm_root(pc), p_sem, NV01_MEMORY_SYSTEM, 0))) {
        d_sem.h = 0;
        goto out;
    }
    d_sem.size = PAGE_4K;
    if (gpu_map(k.c, k.dev, k.vas, &d_sem, SYSMEM_MAP_FLAGS, "producer timeline (dup)"))
        goto out;
    snprintf(w, sizeof w, "copier: DUP_OBJECT source 0x%08x of client 0x%08x", p_src, crm_root(pc));
    if (step(w, crm_dup_object(k.c, k.dev, &d_src.h, crm_root(pc), p_src,
                               o.src_sys ? NV01_MEMORY_SYSTEM : NV01_MEMORY_LOCAL_USER, 0))) {
        d_src.h = 0;
        goto out;
    }
    d_src.size = src_size;
    if (gpu_map(k.c, k.dev, k.vas, &d_src, o.src_sys ? SYSMEM_MAP_FLAGS : 0, "source (dup)"))
        goto out;

    /* Destination: OS descriptor over process pages */
    if (osdesc_alloc(k.c, k.dev, k.vas, &dst, copy_bytes, "destination"))
        goto out;
    printf("precondition: copy_bytes=%" PRIu64 " pages=%" PRIu64
           " (4 KiB pages touched by the destination)\n",
           copy_bytes, (copy_bytes + PAGE_4K - 1) / PAGE_4K);
    printf("copy_bytes=%" PRIu64 "\n", copy_bytes);
    if (osdesc_alloc(k.c, k.dev, k.vas, &stamp, STAMP_SIZE, "timestamps"))
        goto out;

    /* Completion semaphore: RM system memory (a semaphore surface goes over
     * it for the RM fence) */
    if (sysmem_alloc(k.c, k.dev, &done.h, PAGE_4K, "completion semaphore")) {
        done.h = 0;
        goto out;
    }
    done.size = PAGE_4K;
    if (step("copier: CPU-map the completion semaphore",
             crm_map_memory(k.c, k.dev, done.h, 0, PAGE_4K, 0, &done.cpu))) {
        done.cpu = NULL;
        goto out;
    }
    memset(done.cpu, 0, PAGE_4K);
    if (gpu_map(k.c, k.dev, k.vas, &done, SYSMEM_MAP_FLAGS, "completion semaphore"))
        goto out;
#ifdef _WIN32
    if (o.fence) {
        struct semsurf_alloc sa = { .h_semaphore_mem = done.h };
        if (step("copier: NV_SEMAPHORE_SURFACE 0xda over the completion semaphore",
                 crm_alloc(k.c, k.sub, &done_semsurf, NV_SEMAPHORE_SURFACE, &sa, sizeof sa))) {
            done_semsurf = 0;
            goto out;
        }
        NV_SEMAPHORE_SURFACE_CTRL_BIND_CHANNEL_PARAMS bp = {
            .hChannel = ch.ch, .numNotifyIndices = 1, .notifyIndices = { NV2080_NOTIFIERS_FIFO_EVENT_MTHD } };
        if (step("copier: SEMAPHORE_SURFACE BIND_CHANNEL 0xda0002 (FIFO_EVENT_MTHD)",
                 crm_control(k.c, done_semsurf, NV_SEMAPHORE_SURFACE_CTRL_CMD_BIND_CHANNEL, &bp, sizeof bp)))
            goto out;
        semsurf_bound = 1;
        int r = crm_win_open_device(CRM_WIN_DEV_DRI_BASE + 0, &drm_fd);
        if (step("open DRM render node 0", r)) {
            drm_fd = -1;
            goto out;
        }
        if (step("SEMSURF_FENCE_CTX_CREATE on the completion surface",
                 crm_win_semsurf_ctx_create(drm_fd, crm_root(k.c), done_semsurf, PAGE_4K, 0, &fence_ctx)))
            goto out;
    }
#else
    if (o.fence)
        printf("[info] --fence is Windows only (Helios RM fences); ignored\n");
#endif

    volatile uint64_t *cdone = done.cpu;
    volatile uint8_t *st = stamp.cpu;
    volatile uint64_t *probe = (volatile uint64_t *)(st + ST_PROBE);
    volatile uint32_t *t0 = (volatile uint32_t *)(st + ST_T0);
    volatile uint32_t *t1 = (volatile uint32_t *)(st + ST_T1);
    const double tmo = (double)o.timeout_ms * 1000.0;
    uint64_t seq = 0;   /* completion / probe / producer values */
    uint32_t pay = 0;   /* CE semaphore payloads */
    uint64_t last_c = 0; /* last completion value submitted */

    /* First push: SET_OBJECT on the CE subchannel and a release */
    {
        uint32_t *p = slot(&ch);
        unsigned n = 0;
        p[n++] = mthd(SUBC_CE, HOST_SET_OBJECT, 1);
        p[n++] = o.g->ce; /* NVCLASS 15:0, ENGINE 20:16 = 0 */
        n = emit_release(p, n, stamp.va + ST_PROBE, ++seq, 1, 0);
        kick(&k, &ch, n);
        const int ok = spin_ge(probe, seq, now_us() + tmo);
        if (step("ce channel alive: SET_OBJECT + release seen", ok ? 0 : -ETIMEDOUT) || channel_error(&ch)) {
            if (!failed)
                step("ce channel error notifier", -EIO);
            goto out;
        }
    }

    /* Competing channel and its buffers */
    if (o.contend) {
        const int cgr = o.contend_engine < 0;
        if (chan_create(&k, &cch, cgr ? "contend channel (GR CE)" : "contend channel", cgr,
                        cgr ? 0 : NV2080_ENGINE_TYPE_COPY(o.contend_engine)) == 0 &&
            vidmem_alloc(k.c, k.dev, &csrc.h, contend_bytes, "contend source") == 0) {
            csrc.size = contend_bytes;
            if (gpu_map(k.c, k.dev, k.vas, &csrc, 0, "contend source") == 0 &&
                osdesc_alloc(k.c, k.dev, k.vas, &cdst, contend_bytes, "contend destination") == 0) {
                uint32_t *p = slot(&cch);
                unsigned n = 0;
                p[n++] = mthd(SUBC_CE, HOST_SET_OBJECT, 1);
                p[n++] = o.g->ce;
                n = emit_release(p, n, stamp.va + ST_K, ++seq, 1, 0);
                kick(&k, &cch, n);
                const int ok = spin_ge((volatile uint64_t *)(st + ST_K), seq, now_us() + tmo);
                contend_ok = !step("contend channel alive", ok ? 0 : -ETIMEDOUT) && !channel_error(&cch);
                if (contend_ok)
                    printf("       runlists: ce channel %u, contend channel %u (%s)\n",
                           (ch.token >> 16) & 0x7fu, (cch.token >> 16) & 0x7fu,
                           ((ch.token >> 16) & 0x7fu) == ((cch.token >> 16) & 0x7fu)
                               ? "SAME runlist: time-sliced"
                               : "separate runlists: can run concurrently");
            }
        }
        if (!contend_ok)
            goto out;
    }

    if (o.delay_s > 0) {
        printf("status: setup done, waiting %.1f s before the measured loop\n", o.delay_s);
        fflush(stdout);
        const double end = now_us() + o.delay_s * 1e6;
        while (now_us() < end)
            sleep_ms(100);
    }

    const double loop_start = now_us();
    const double loop_end = o.duration_s > 0 ? loop_start + o.duration_s * 1e6 : 0;
    printf("status: measured loop starts (%s)\n", o.duration_s > 0 ? "duration" : "iterations");
    if (o.duration_s > 0)
        printf("       duration %.1f s\n", o.duration_s);
    else
        printf("       %u iterations\n", o.iterations);
    fflush(stdout);

    const char *stop = NULL;
    unsigned it;
    for (it = 0;; it++) {
        if (loop_end > 0 ? now_us() >= loop_end : it >= o.iterations)
            break;
        const int verify = (it % 16) == 0;
        uint32_t *p;
        unsigned n;

        /* ready: producer value set before the doorbell */
        {
            const uint64_t v = ++seq, cval = v;
            last_c = cval;
            const uint32_t py = ++pay;
            *prod = v;
            full_fence();
            if (verify)
                memset(dst.cpu, 0, copy_bytes);
            p = slot(&ch);
            n = emit_host_sem(p, 0, d_sem.va, v,
                              SEM_EXECUTE_OPERATION_ACQ_STRICT_GEQ | SEM_EXECUTE_ACQUIRE_SWITCH_TSG_EN |
                                  SEM_EXECUTE_PAYLOAD_SIZE_64BIT);
            n = emit_ce_copy(p, n, d_src.va, dst.va, pitch, o.height, stamp.va + ST_T0, stamp.va + ST_T1, py);
            n = emit_release(p, n, done.va, cval, 1, 1);
#ifdef _WIN32
            int fence = -1;
            if (o.fence) {
                int r = crm_win_semsurf_fence_create(drm_fd, fence_ctx, cval, o.timeout_ms, &fence);
                if (r || crm_win_fence_wait(fence, 0) < 0) {
                    step("SEMSURF_FENCE_CREATE + EVENT_REGISTER", r ? r : -EIO);
                    if (fence >= 0)
                        crm_win_close_device(fence);
                    stop = "fence";
                    break;
                }
            }
#endif
            const double a = now_us();
            kick(&k, &ch, n);
            const int ok = spin_ge(cdone, cval, a + tmo);
            const double b = now_us();
#ifdef _WIN32
            if (o.fence) {
                const int fired = crm_win_fence_wait(fence, o.timeout_ms);
                const double e = now_us();
                if (fired == 1)
                    add(&s_event, e - a);
                crm_win_close_device(fence);
            }
#endif
            if (!ok) {
                stop = "ready: completion not seen";
                break;
            }
            add(&s_ready, b - a);
            if (spin_ge32(t1, py, b + tmo) && spin_ge32(t0, py, b + tmo)) {
                const uint64_t ts0 = *(volatile uint64_t *)(st + ST_T0 + 8);
                const uint64_t ts1 = *(volatile uint64_t *)(st + ST_T1 + 8);
                if (ts1 >= ts0)
                    add(&s_copy, (double)(ts1 - ts0) / 1000.0);
            }
            if (verify) {
                const volatile uint32_t *dw = dst.cpu;
                uint64_t bad = 0;
                for (size_t i = 0; i < copy_bytes / 4; i++)
                    bad += dw[i] != pattern(i);
                bad_bytes += bad * 4;
                verified++;
            }
            const uint32_t g = ch.userd[USERD_GP_GET / 4];
            if (g)
                gp_get_seen = g;
        }

        /* wait: submit first, the CPU satisfies the acquire after hold_ms */
        {
            const uint64_t v = ++seq, cval = v;
            last_c = cval;
            const uint32_t py = ++pay;
            p = slot(&ch);
            n = emit_host_sem(p, 0, d_sem.va, v,
                              SEM_EXECUTE_OPERATION_ACQ_STRICT_GEQ | SEM_EXECUTE_ACQUIRE_SWITCH_TSG_EN |
                                  SEM_EXECUTE_PAYLOAD_SIZE_64BIT);
            n = emit_ce_copy(p, n, d_src.va, dst.va, pitch, o.height, stamp.va + ST_T0, stamp.va + ST_T1, py);
            n = emit_release(p, n, done.va, cval, 1, 1);
            kick(&k, &ch, n);
            sleep_ms(o.hold_ms);
            wait_rounds++;
            if (*cdone < cval && (int32_t)(*t0 - py) < 0)
                held++;
            double a;
            if (o.release_semsurf) {
                struct semsurf_set_value sv = { .index = 0, .new_value = v };
                a = now_us();
                int r = crm_control(pc, p_semsurf, NV_SEMAPHORE_SURFACE_CTRL_CMD_SET_VALUE, &sv, sizeof sv);
                add(&s_setval, now_us() - a);
                if (r) {
                    step("NV_SEMAPHORE_SURFACE_CTRL_CMD_SET_VALUE 0xda0004", r);
                    *prod = v; /* never leave the GPU waiting */
                    stop = "wait: SET_VALUE";
                    spin_ge(cdone, cval, now_us() + tmo);
                    break;
                }
            } else {
                a = now_us();
                *prod = v;
                full_fence();
            }
            const int ok = spin_ge(cdone, cval, a + tmo);
            const double b = now_us();
            if (!ok) {
                stop = "wait: completion not seen after the release";
                break;
            }
            add(&s_wait, b - a);
            if (spin_ge32(t1, py, b + tmo)) {
                const uint64_t ts0 = *(volatile uint64_t *)(st + ST_T0 + 8);
                const uint64_t ts1 = *(volatile uint64_t *)(st + ST_T1 + 8);
                if (ts1 >= ts0)
                    add(&s_copy_wait, (double)(ts1 - ts0) / 1000.0);
            }
        }

        /* probe: doorbell -> the first release (no WFI) */
        {
            const uint64_t v = ++seq;
            p = slot(&ch);
            n = emit_release(p, 0, stamp.va + ST_PROBE, v, 0, 0);
            const double a = now_us();
            kick(&k, &ch, n);
            const int ok = spin_ge(probe, v, a + tmo);
            const double b = now_us();
            if (!ok) {
                stop = "probe: release not seen";
                break;
            }
            add(&s_probe, b - a);
        }

        /* contended: the competing copy in flight while a ready round runs */
        if (o.contend) {
            const uint64_t kv = ++seq;
            const uint32_t kpy = ++pay;
            const uint32_t cpitch = 16384u;
            uint32_t *q = slot(&cch);
            unsigned m = emit_ce_copy(q, 0, csrc.va, cdst.va, cpitch, (uint32_t)(contend_bytes / cpitch),
                                      stamp.va + ST_K_T0, stamp.va + ST_K_T1, kpy);
            m = emit_release(q, m, stamp.va + ST_K, kv, 1, 0);
            const double ka = now_us();
            kick(&k, &cch, m);
            /* let it start before the measured copy */
            spin_ge32((volatile uint32_t *)(st + ST_K_T0), kpy, ka + tmo);

            const uint64_t v = ++seq, cval = v;
            last_c = cval;
            const uint32_t py = ++pay;
            *prod = v;
            full_fence();
            p = slot(&ch);
            n = emit_host_sem(p, 0, d_sem.va, v,
                              SEM_EXECUTE_OPERATION_ACQ_STRICT_GEQ | SEM_EXECUTE_ACQUIRE_SWITCH_TSG_EN |
                                  SEM_EXECUTE_PAYLOAD_SIZE_64BIT);
            n = emit_ce_copy(p, n, d_src.va, dst.va, pitch, o.height, stamp.va + ST_T0, stamp.va + ST_T1, py);
            n = emit_release(p, n, done.va, cval, 1, 1);
            const double a = now_us();
            kick(&k, &ch, n);
            const int ok = spin_ge(cdone, cval, a + tmo);
            const double b = now_us();
            const int other_running = *(volatile uint64_t *)(st + ST_K) < kv;
            const int kok = spin_ge((volatile uint64_t *)(st + ST_K), kv, now_us() + tmo);
            const double kb = now_us();
            if (!ok || !kok) {
                stop = !ok ? "contended: completion not seen" : "contended: competing copy not done";
                break;
            }
            contended++;
            overlapped += other_running;
            add(&s_c_ready, b - a);
            add(&s_c_other, kb - ka);
            if (spin_ge32(t1, py, b + tmo)) {
                const uint64_t ts0 = *(volatile uint64_t *)(st + ST_T0 + 8);
                const uint64_t ts1 = *(volatile uint64_t *)(st + ST_T1 + 8);
                if (ts1 >= ts0)
                    add(&s_c_copy, (double)(ts1 - ts0) / 1000.0);
            }
            if (channel_error(&cch)) {
                stop = "contend channel error";
                break;
            }
        }

        if (channel_error(&ch)) {
            stop = "ce channel error";
            break;
        }
    }
    printf("status: measured loop ends after %u iterations, %.1f s\n", it, (now_us() - loop_start) / 1e6);

    if (stop) {
        /* Bounded recovery: release the producer far ahead, so no acquire can
         * hold the GPU, give the channel one more timeout, then tear down. */
        *prod = seq + (1ull << 32);
        full_fence();
        spin_ge(cdone, last_c, now_us() + tmo);
        channel_error(&ch);
        if (o.contend)
            channel_error(&cch);
        printf("       completion 0x%" PRIx64 ", producer 0x%" PRIx64 ", probe 0x%" PRIx64
               ", USERD GP_PUT %u GP_GET %u\n",
               (uint64_t)*cdone, (uint64_t)*prod, (uint64_t)*probe, ch.userd[USERD_GP_PUT / 4],
               ch.userd[USERD_GP_GET / 4]);
        step(stop, -ETIMEDOUT);
    }

    printf("\n%-38s %6s %10s %10s %10s %10s %10s\n", "stage", "n", "min", "avg", "p50", "p99", "max");
    stats_row(&s_ready);
    stats_row(&s_wait);
    stats_row(&s_probe);
    stats_row(&s_copy);
    stats_row(&s_copy_wait);
    if (o.fence)
        stats_row(&s_event);
    if (o.release_semsurf)
        stats_row(&s_setval);
    if (o.contend) {
        stats_row(&s_c_ready);
        stats_row(&s_c_copy);
        stats_row(&s_c_other);
    }
    printf("\n");
    printf("copy_bytes=%" PRIu64 "\n", copy_bytes);
    if (s_copy.n) {
        /* s_copy is sorted by stats_row */
        printf("copy_gbps_p50=%.2f\n", (double)copy_bytes / (s_copy.v[s_copy.n / 2] * 1000.0));
    }
    printf("acquire_held=%u/%u\n", held, wait_rounds);
    if (o.contend)
        printf("contend_overlapped=%u/%u\n", overlapped, contended);
    printf("userd_gp_get_written_back=%s\n", gp_get_seen ? "yes" : "no");
    printf("verify=%s (%u checks, %" PRIu64 " bad bytes)\n", bad_bytes == 0 && verified ? "ok" : "BAD",
           verified, bad_bytes);
    if (!stop) {
        if (wait_rounds && held != wait_rounds)
            step("acquire held in every wait round", -EIO);
        if (bad_bytes || !verified)
            step("destination matches the pattern", -EIO);
        if (ch.notifier && ch.notifier[0].status)
            step("error notifier stays 0", -EIO);
    }

out:
    /* Teardown in reverse order; the channels go first, so the GPU stops
     * referencing memory before it is freed. */
    if (k.c) {
#ifdef _WIN32
        if (drm_fd >= 0)
            crm_win_close_device(drm_fd); /* frees the fence context's GEM handle too */
#endif
        if (semsurf_bound) {
            NV_SEMAPHORE_SURFACE_CTRL_BIND_CHANNEL_PARAMS bp = {
                .hChannel = ch.ch, .numNotifyIndices = 1, .notifyIndices = { NV2080_NOTIFIERS_FIFO_EVENT_MTHD } };
            crm_control(k.c, done_semsurf, NV_SEMAPHORE_SURFACE_CTRL_CMD_UNBIND_CHANNEL, &bp, sizeof bp);
        }
        chan_destroy(&k, &cch);
        chan_destroy(&k, &ch);
        if (done_semsurf)
            crm_free(k.c, k.sub, done_semsurf);
        gpu_unmap(k.c, k.dev, k.vas, &done);
        if (done.cpu)
            crm_unmap_memory(k.c, k.dev, done.h, done.cpu, PAGE_4K, 0);
        if (done.h)
            crm_free(k.c, k.dev, done.h);
        osdesc_free(k.c, k.dev, k.vas, &cdst);
        gpu_unmap(k.c, k.dev, k.vas, &csrc);
        if (csrc.h)
            crm_free(k.c, k.dev, csrc.h);
        osdesc_free(k.c, k.dev, k.vas, &stamp);
        osdesc_free(k.c, k.dev, k.vas, &dst);
        gpu_unmap(k.c, k.dev, k.vas, &d_src);
        if (d_src.h)
            crm_free(k.c, k.dev, d_src.h);
        gpu_unmap(k.c, k.dev, k.vas, &d_sem);
        if (d_sem.h)
            crm_free(k.c, k.dev, d_sem.h);
        if (k.um_map)
            crm_unmap_memory(k.c, k.sub, k.um, k.um_map, USERMODE_SIZE, 0);
        if (k.um)
            crm_free(k.c, k.sub, k.um);
        if (k.vas)
            crm_free(k.c, k.dev, k.vas);
        if (k.sub)
            crm_free(k.c, k.dev, k.sub);
        if (k.dev)
            crm_free(k.c, crm_root(k.c), k.dev);
        const size_t objs = crm_object_count(k.c), maps = crm_mapping_count(k.c);
        printf("       copier: objects still tracked %zu, CPU mappings %zu\n", objs, maps);
        if (objs || maps)
            step("copier teardown leaves nothing tracked", -EBUSY);
        crm_close(k.c);
    }
    if (pc) {
        if (p_src_map)
            crm_unmap_memory(pc, p_dev, p_src, p_src_map, src_size, 0);
        if (p_src)
            crm_free(pc, p_dev, p_src);
        if (p_semsurf)
            crm_free(pc, p_sub, p_semsurf);
        if (p_sem_map)
            crm_unmap_memory(pc, p_dev, p_sem, p_sem_map, PAGE_4K, 0);
        if (p_sem)
            crm_free(pc, p_dev, p_sem);
        if (p_sub)
            crm_free(pc, p_dev, p_sub);
        if (p_dev)
            crm_free(pc, crm_root(pc), p_dev);
        const size_t objs = crm_object_count(pc), maps = crm_mapping_count(pc);
        printf("       producer: objects still tracked %zu, CPU mappings %zu\n", objs, maps);
        if (objs || maps)
            step("producer teardown leaves nothing tracked", -EBUSY);
        crm_close(pc);
    }
    free(s_ready.v);
    free(s_wait.v);
    free(s_probe.v);
    free(s_copy.v);
    free(s_copy_wait.v);
    free(s_event.v);
    free(s_c_ready.v);
    free(s_c_copy.v);
    free(s_c_other.v);
    free(s_setval.v);
    if (failed)
        printf("RESULT FAIL %s: %s (0x%x)\n", fail_what, crm_status_name(fail_status), (unsigned)fail_status);
    else
        printf("RESULT PASS\n");
    return failed ? 1 : 0;
}
