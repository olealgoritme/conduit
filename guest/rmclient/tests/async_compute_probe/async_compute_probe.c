/* SPDX-License-Identifier: MIT */
/*
 * async_compute_probe: can a compute-only channel run next to the 3D channel
 * on the GR engine of a GB202, through the RM channel path NVK-on-RM uses
 * (no Vulkan: librmclient only).
 *
 * Question: NVK exposes one universal queue family, so vkd3d-proton's D3D12
 * COMPUTE queues run behind DIRECT.  An extra compute queue family needs a
 * second GPFIFO channel that executes concurrently with the first.  On
 * Volta and later the hardware unit of concurrency inside one GR context is
 * the VEID (subcontext): VEID 0 (SYNC) runs graphics and compute, VEID > 0
 * (ASYNC) runs compute only; channels of different TSGs on one runlist are
 * only time-sliced.  nvos.h (NV_CTXSHARE_ALLOCATION_PARAMETERS) and
 * kernel_ctxshare.c / kernel_channel_group_api.c of the open kernel modules
 * describe the ctxshare side; nvkmd_rm_ctx.c creates one TSG per context
 * with a single SYNC ctxshare today.
 *
 * How NVIDIA's own Vulkan driver does it (610.57.04, GB202, observed with an
 * ioctl trace of its RM calls): each graphics queue is a TSG with a VEID 0
 * channel (3D, compute, copy, 2D, I2M objects) plus a compute-only channel on
 * a SPECIFIED VEID 1; each compute queue adds a compute-only channel to the
 * FIRST graphics queue's TSG on SPECIFIED VEID 2, 3, ...  Every compute-only
 * channel is allocated with NV_CHANNEL_ALLOC_PARAMS.flags = GROUP_CHANNEL_
 * RUNQUEUE_ONE | GROUP_CHANNEL_THREAD_ONE (0x10000010).  A compute queue then
 * runs concurrently with graphics queue 0 (and with other compute queues),
 * and time-sliced against graphics queue 1 (another TSG).
 *
 * Scenarios (each creates its own TSGs / ctxshares / channels on the GR
 * engine, runs them, tears them down; a refusal is a result, not an abort):
 *
 *   async        one TSG: SYNC ctxshare (VEID 0) + channel A with 3D, compute
 *                and copy objects (what NVK's queue is), SPECIFIED VEID 1
 *                ctxshare + channel B (RUNQUEUE_ONE|THREAD_ONE) with a
 *                compute object: NVIDIA's layout.  A copy object on B is
 *                tried afterwards (CE copies on a compute queue need it).
 *   async-late   as async, but B is added after the TSG is scheduled
 *                (nothing else is called for B; if B does not run,
 *                NVA06F_CTRL_CMD_GPFIFO_SCHEDULE is tried on B).
 *   async3       SYNC + SPECIFIED VEIDs 1 and 2, three channels.
 *   sync2        one TSG, ONE SYNC ctxshare, channels A and B on it: shows
 *                whether two channels alone overlap without a second VEID.
 *   tsg2         two TSGs, A in one, B in the other (what a second NVK
 *                context is today): expected time-sliced, not concurrent.
 *   async3d      VEID 1 channel with a 3D object: graphics runs on VEID 0
 *                only (status recorded; nothing is run).
 *   async-old    the first probe's layout (ASYNC ctxshare -> VEID 63, channel
 *                flags 0): compute object refused with NV_ERR_INVALID_OBJECT.
 *   m-*          creation-only variants that separate the two differences
 *                (VEID choice, channel flags); "--scenario matrix" runs all
 *                creation-only scenarios and submits no GPU work.
 *
 * Work: a compute shader (spin.comp, NAK SM120 machine code in spin_sass.h)
 * that busy-waits SPIN_CYCLES SM clocks per workgroup (one warp each) and
 * stores a marker at a literal output address.  A launch is --wgs
 * workgroups, repeated --k times per channel, so a channel is busy for about
 * k * 0.5 ms while occupying only a few SMs.  Per channel the push is
 *
 *   host SEM_EXECUTE ACQ_STRICT_GEQ on the gate (the CPU opens it once every
 *     channel has been kicked: all channels start within microseconds),
 *   host release with timestamp (T0, WFI),
 *   k x { SEND_PCAS_A (QMD address >> 8), SEND_SIGNALING_PCAS2_B },
 *   host release with timestamp (T1, WFI),
 *
 * T0/T1 are GPU timer nanoseconds (the second half of a 16-byte semaphore
 * release, as uvm_hal_hopper_host_semaphore_timestamp does), the same clock
 * for every channel.  T0 is taken right after the channel passes the gate,
 * before its kernels, so T0 alone says nothing about when the kernels ran;
 * the verdict therefore uses completion times.  A channel alone gives its
 * solo duration (T1 - T0).  All channels gated together finish in about the
 * longest solo duration if they overlap and in about the sum if they are
 * serialised:
 *
 *   serial fraction = (makespan - longest solo) / (sum of solos - longest solo)
 *   makespan = max(T1) - min(T0), medians over --reps trials
 *   CONCURRENT  overlapped = 1 - serial fraction >= 75%
 *   SERIAL      overlapped <= 25% (time slicing lands here too)
 *   PARTIAL     in between
 *
 * --xwait adds a dependency test: channel B acquires a semaphore that A
 * releases after its work, with and without ACQUIRE_SWITCH_TSG on B's
 * acquire (nvkmd_rm_ctx.c sets it on every wait today); the hand-off
 * T0(B) - T1(A) and the makespan are reported.  --contend (implies --xwait)
 * runs it while a channel in a TSG of its own keeps spinning, so that a TSG
 * switch has another runnable TSG to switch to (an idle GPU hides the cost).
 *
 * Every CPU wait is bounded (--timeout-ms).  A wait that times out opens the
 * gate, reads the error notifiers and ends the scenario; teardown disables
 * the TSG's schedule and frees the TSG (channels and objects with it) before
 * any memory the channels reference.  A bad method raises an RC error on the
 * probe's own channel (printed as "channel error notifier"), not a GPU hang.
 *
 * Sources of the encodings:
 *   host (*6F) SEM_ADDR_LO..SEM_EXECUTE 0x5c.., OPERATION 2:0 (RELEASE 1,
 *   ACQ_STRICT_GEQ 2), ACQUIRE_SWITCH_TSG 12, RELEASE_WFI 20, PAYLOAD_SIZE
 *   24 (64BIT 1), RELEASE_TIMESTAMP 25: clc56f.h / clc86f.h of Mesa's
 *   nvidia headers (identical in the 615.78.08 open kernel modules).
 *   Compute (cec0): SET_OBJECT 0x0, SET_SHADER_SHARED_MEMORY_WINDOW_A/B
 *   0x2a0/0x2a4, SEND_PCAS_A 0x2b4, SEND_SIGNALING_PCAS2_B 0x2c0 (action 3
 *   INVALIDATE_COPY_SCHEDULE), INVALIDATE_SKED_CACHES 0x298,
 *   SET_SHADER_LOCAL_MEMORY_WINDOW_A/B 0x7b0/0x7b4: clcec0.h; the sequence
 *   is nvk_push_dispatch_state_init and nvk_CmdDispatchBase.
 *   QMD V05_00: clcdc0qmd.h with the field choices of Mesa's nak/qmd.rs
 *   Qmd5_0 (fill_qmd); shared-memory config for sm120 with no shared
 *   memory: MIN = TARGET = 1, MAX = 26 (sizes 0,8,16,32,64,100 KB).
 *   Channel / TSG / ctxshare / BIND / token / SCHEDULE calls: nvkmd_rm_ctx.c
 *   (nvkmd_rm_create_exec_ctx) and crm_ce_copy_smoke.c.
 *
 * Usage: async_compute_probe [options]
 *   --scenario <list>   comma list of: async,async-late,async3,sync2,tsg2,
 *                       async3d, all (default all)
 *   --reps <n>          trials per measurement (default 15)
 *   --wgs <n>           workgroups per launch (default 16, max 4096)
 *   --k <n>             launches per channel per trial (default 4, max 12)
 *   --xwait             also run the dependency test (first two channels)
 *   --contend           the dependency test next to a busy channel in another TSG
 *   --timeout-ms <ms>   bound of every CPU wait (default 3000)
 *   --verbose           print every trial
 * Exit 0 when the async scenario created its channels, both ran and every
 * output marker was written (without async selected: when every selected
 * scenario that runs work did); 1 otherwise.  CONCURRENT or SERIAL is the
 * finding, not a pass/fail criterion.
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

#include "spin_sass.h"

/* The QMD layout and the compute class methods come from NVIDIA's own
 * headers (MIT): copied next to this file. */
#define MW(i) i
#include "clcdc0qmd.h"

/* ---- classes, controls, parameters (nvkmd_rm_ctx.c, ce_copy_smoke) ---- */

#define FERMI_CONTEXT_SHARE_A 0x00009067u
#define KEPLER_CHANNEL_GROUP_A 0x0000a06cu

#define NV2080_ENGINE_TYPE_GRAPHICS 0x00000001u

#define NV_VASPACE_ALLOCATION_INDEX_GPU_DEVICE 3u
#define NV_DEVICE_ALLOCATION_FLAGS_VASPACE_BIG_PAGE_SIZE_64k 0x00000200u
#define NV_DEVICE_ALLOCATION_VAMODE_OPTIONAL_MULTIPLE_VASPACES 0u
#define NV_CTXSHARE_ALLOCATION_FLAGS_SUBCONTEXT_SYNC 0u
#define NV_CTXSHARE_ALLOCATION_FLAGS_SUBCONTEXT_ASYNC 1u
#define NV_CTXSHARE_ALLOCATION_FLAGS_SUBCONTEXT_SPECIFIED 2u
#define NV_CTXSHARE_ALLOCATION_FLAGS_SUBCONTEXT_ASYNC_PREFER_LOWER 3u
/* NV_CHANNEL_ALLOC_PARAMS.flags (alloc_channel.h NVOS04_FLAGS_*): GROUP_CHANNEL_RUNQUEUE 4:4,
 * GROUP_CHANNEL_THREAD 29:28.  NVIDIA's Vulkan driver allocates every compute-only channel of a
 * VEID > 0 with RUNQUEUE_ONE | THREAD_ONE (0x10000010); kfifoValidateSCGTypeAndRunqueue_GB202
 * refuses THREAD 1 on runqueue 0. */
#define NVOS04_FLAGS_GROUP_CHANNEL_RUNQUEUE_ONE (1u << 4)
#define NVOS04_FLAGS_GROUP_CHANNEL_THREAD_ONE (1u << 28)
#define CHF_ASYNC (NVOS04_FLAGS_GROUP_CHANNEL_RUNQUEUE_ONE | NVOS04_FLAGS_GROUP_CHANNEL_THREAD_ONE)
#define NV_CHANNELGPFIFO_NOTIFICATION_TYPE_ERROR 0u
#define NV_CHANNELGPFIFO_NOTIFICATION_TYPE__SIZE_1 3u

#define NVA06C_CTRL_CMD_GPFIFO_SCHEDULE 0xa06c0101u
#define NVA06F_CTRL_CMD_GPFIFO_SCHEDULE 0xa06f0103u
#define NVA06F_CTRL_CMD_BIND 0xa06f0104u
#define NVC36F_CTRL_CMD_GPFIFO_GET_WORK_SUBMIT_TOKEN 0xc36f0108u
#define NVC36F_CTRL_CMD_GPFIFO_SET_WORK_SUBMIT_TOKEN_NOTIF_INDEX 0xc36f010au
#define NV2080_CTRL_CMD_FIFO_GET_INFO 0x20801109u
#define NV2080_CTRL_FIFO_INFO_INDEX_MAX_SUBCONTEXT_PER_GROUP 0x3u
#define NV2080_CTRL_FIFO_INFO_INDEX_MAX_LOWER_SUBCONTEXT 0xau

#define NVOS46_FLAGS_CACHE_SNOOP_ENABLE (1u << 4)
#define NVOS46_FLAGS_PAGE_SIZE_4KB (1u << 8)
#define SYSMEM_MAP_FLAGS (NVOS46_FLAGS_CACHE_SNOOP_ENABLE | NVOS46_FLAGS_PAGE_SIZE_4KB)

/* GB202 classes (nvkmd_rm_pdev.c pick_class: eng3d 0xce97, compute 0xcec0,
 * copy 0xcab5, gpfifo 0xca6f, usermode 0xc761) */
#define CLS_GPFIFO 0xca6fu
#define CLS_3D 0xce97u
#define CLS_COMPUTE 0xcec0u
#define CLS_COPY 0xcab5u
#define CLS_USERMODE 0xc761u

#include "rm_params.h"

/* ---- methods ---- */

#define HOST_SET_OBJECT 0x0000u
#define HOST_SEM_ADDR_LO 0x005cu
#define SEM_OP_RELEASE 0x1u
#define SEM_OP_ACQ_STRICT_GEQ 0x2u
#define SEM_SWITCH_TSG (1u << 12)
#define SEM_RELEASE_WFI (1u << 20)
#define SEM_PAYLOAD_64 (1u << 24)
#define SEM_RELEASE_TIMESTAMP (1u << 25)

#define CMP_SET_OBJECT 0x0000u
#define CMP_INVALIDATE_SKED_CACHES 0x0298u
#define CMP_SET_SHADER_SHARED_MEMORY_WINDOW_A 0x02a0u
#define CMP_SEND_PCAS_A 0x02b4u
#define CMP_SEND_SIGNALING_PCAS2_B 0x02c0u
#define CMP_PCAS_ACTION_INVALIDATE_COPY_SCHEDULE 3u
#define CMP_SET_SHADER_LOCAL_MEMORY_WINDOW_A 0x07b0u

/* DMA copy (*B5), clcab5.h / crm_ce_copy_smoke.c */
#define CE_SET_OBJECT 0x0000u
#define CE_LAUNCH_DMA 0x0300u
#define CE_OFFSET_IN_UPPER 0x0400u
#define CE_LAUNCH_NON_PIPELINED 0x2u
#define CE_LAUNCH_FLUSH_ENABLE (1u << 2)
#define CE_LAUNCH_SRC_PITCH (1u << 7)
#define CE_LAUNCH_DST_PITCH (1u << 8)
#define CE_LAUNCH_MULTI_LINE (1u << 9)
#define SUBC_CE 4u /* nv_push.h SUBC_NV90B5 */

#define SUBC_HOST 0u /* host methods ignore the subchannel (nvk-rm 0006) */
#define SUBC_COMPUTE 1u /* nv_push.h SUBC_NVC7C0 */

#define USERD_GP_PUT 0x8cu
#define USERMODE_NOTIFY_CHANNEL_PENDING 0x90u
#define USERMODE_SIZE 0x10000u

/* ---- layout ---- */

#define GPFIFO_ENTRIES 128u
#define PUSH_SLOT_B 1024u
#define PUSH_SLOT_DW (PUSH_SLOT_B / 4u)
#define RING_PUSH_OFFSET 4096u
#define RING_SIZE (192u * 1024u)
#define CTL_SIZE 8192u
#define CTL_USERD_OFFSET 4096u
#define PAGE_4K 4096u
#define MIB (1024ull * 1024ull)
#define VA_BASE 0x2000000000ull /* below 2^40: GP_ENTRY1_GET_HI is 8 bits */

#define MAX_CH 3u
#define MAX_K 12u
#define MAX_WGS 4096u
#define OUT_SIZE (64u * 1024u)

/* stamps page: gate at 0, per channel a block of 256 B at 256 + 256 * i:
 * T0 slot +0, T1 slot +16 (payload32, pad, timestamp64), X semaphore +32 */
#define ST_GATE 0u
#define ST_CH(i) (256u + 256u * (i))
#define ST_T0 0u
#define ST_T1 16u
#define ST_X 32u

/* work region: code per variant at 4096 * v, constant buffer 0 at 0x3000,
 * QMDs at 0x4000 + 512 * (channel * MAX_K + k) */
#define WORK_SIZE (64u * 1024u)
#define WORK_CODE(v) (4096u * (v))
#define WORK_CB0 0x3000u
#define WORK_QMD(ch, k) (0x4000u + 512u * ((ch) * MAX_K + (k)))

/* ---- small helpers ---- */

static unsigned timeout_ms = 3000;
static int verbose;

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

static int say(int r, const char *what)
{
    if (r == 0)
        printf("[ ok ] %s\n", what);
    else
        printf("[FAIL] %s: %s (%d / 0x%x) %s\n", what, crm_status_name(r), r, (unsigned)r,
               crm_status_string(r));
    fflush(stdout);
    return r;
}

static int cmp_d(const void *a, const void *b)
{
    const double x = *(const double *)a, y = *(const double *)b;
    return x < y ? -1 : x > y;
}
static double median(double *v, unsigned n)
{
    if (n == 0)
        return 0;
    qsort(v, n, sizeof(double), cmp_d);
    return (n & 1) ? v[n / 2] : 0.5 * (v[n / 2 - 1] + v[n / 2]);
}

/* ---- device ---- */

struct gmem {
    uint32_t h;
    uint64_t size, va;
    void *pages;
    uint64_t pages_size;
    void *cpu;
};

static struct {
    crm_client *c;
    uint32_t dev, sub, vas, um;
    void *um_map;
} D;

static uint64_t next_va = VA_BASE;

static int gpu_map_flags(struct gmem *m, uint32_t flags, uint64_t want_va, const char *what)
{
    char w[320];
    uint64_t va = want_va ? want_va : next_va;
    int r = crm_map_dma2(D.c, D.dev, D.vas, m->h, 0, m->size, flags, 0, 0, &va);
    if (r > 0 && !want_va) {
        va = 0;
        r = crm_map_dma2(D.c, D.dev, D.vas, m->h, 0, m->size, flags, 0, 0, &va);
    }
    snprintf(w, sizeof w, "GPU-map %s (%" PRIu64 " bytes) -> VA 0x%" PRIx64, what, m->size, va);
    if (say(r, w))
        return r;
    m->va = va;
    if (!want_va && va == next_va)
        next_va += align_up(m->size, 2 * MIB) + 2 * MIB;
    return 0;
}

static void gpu_unmap(struct gmem *m)
{
    if (m->va)
        crm_unmap_dma(D.c, D.dev, D.vas, m->h, 0, m->va);
    m->va = 0;
}

/* OS descriptor over process pages, GPU-mapped snooped, optionally at a fixed VA */
static int osdesc_alloc(struct gmem *m, uint64_t size, uint64_t want_va, const char *what)
{
    char w[320];
    memset(m, 0, sizeof *m);
    m->pages_size = align_up(size, 64 * 1024);
    snprintf(w, sizeof w, "crm_alloc_pages %" PRIu64 " bytes (%s)", m->pages_size, what);
    if (say(crm_alloc_pages(D.c, m->pages_size, &m->pages), w)) {
        m->pages = NULL;
        return -1;
    }
    memset(m->pages, 0, m->pages_size);
    snprintf(w, sizeof w, "alloc NV01_MEMORY_SYSTEM_OS_DESCRIPTOR (%s)", what);
    if (say(crm_alloc_os_descriptor(D.c, D.dev, &m->h, m->pages, m->pages_size, 0), w)) {
        m->h = 0;
        return -1;
    }
    m->size = m->pages_size;
    m->cpu = m->pages;
    return gpu_map_flags(m, SYSMEM_MAP_FLAGS, want_va, what);
}

static void osdesc_free(struct gmem *m)
{
    gpu_unmap(m);
    if (m->h)
        crm_free(D.c, D.dev, m->h);
    if (m->pages)
        crm_free_pages(D.c, m->pages, m->pages_size);
    memset(m, 0, sizeof *m);
}

static int sysmem_alloc(uint32_t *h, uint64_t size, const char *what)
{
    char w[320];
    NV_MEMORY_ALLOCATION_PARAMS mp;
    memset(&mp, 0, sizeof mp);
    mp.owner = crm_root(D.c);
    mp.type = NVOS32_TYPE_IMAGE;
    mp.attr = (NVOS32_ATTR_LOCATION_PCI << NVOS32_ATTR_LOCATION_SHIFT) |
              (NVOS32_ATTR_PAGE_SIZE_4KB << NVOS32_ATTR_PAGE_SIZE_SHIFT) |
              (NVOS32_ATTR_PHYSICALITY_NONCONTIGUOUS << NVOS32_ATTR_PHYSICALITY_SHIFT) |
              (NVOS32_ATTR_COHERENCY_CACHED << NVOS32_ATTR_COHERENCY_SHIFT);
    mp.size = size;
    snprintf(w, sizeof w, "alloc NV01_MEMORY_SYSTEM %" PRIu64 " bytes (%s)", size, what);
    return say(crm_alloc(D.c, D.dev, h, NV01_MEMORY_SYSTEM, &mp, sizeof mp), w);
}

/* ---- channels ---- */

struct tsg {
    uint32_t h;
    int scheduled;
};

struct cshare {
    uint32_t h;
    uint32_t veid; /* subctxId RM returned */
};

struct chan {
    char label[24];
    int tsg, cs; /* indices into the scenario's arrays */
    uint32_t obj_mask; /* bit 0 3D, bit 1 compute, bit 2 copy */
    uint32_t chflags;  /* NV_CHANNEL_ALLOC_PARAMS.flags */
    uint32_t ch, objs[3];
    struct gmem ring;
    uint32_t ctl;
    void *ctl_map;
    volatile nv_notification *notifier;
    volatile uint32_t *userd;
    uint32_t token, put;
    int created;
};

enum { OBJ_3D = 1, OBJ_COMPUTE = 2, OBJ_COPY = 4 };

static uint32_t mthd(uint32_t subc, uint32_t m, uint32_t count)
{
    return (1u << 29) | (count << 16) | (subc << 13) | (m >> 2);
}

static int tsg_create(struct tsg *t, const char *label)
{
    char w[320];
    NV_CHANNEL_GROUP_ALLOCATION_PARAMETERS tp;
    memset(&tp, 0, sizeof tp);
    tp.hVASpace = D.vas;
    tp.engineType = NV2080_ENGINE_TYPE_GRAPHICS;
    snprintf(w, sizeof w, "%s: alloc KEPLER_CHANNEL_GROUP_A {GR}", label);
    t->scheduled = 0;
    return say(crm_alloc(D.c, D.dev, &t->h, KEPLER_CHANNEL_GROUP_A, &tp, sizeof tp), w);
}

static const char *cs_name(uint32_t flags)
{
    static const char *n[4] = { "SYNC", "ASYNC", "SPECIFIED", "ASYNC_PREFER_LOWER" };
    return n[flags & 3];
}

/* subctx: the VEID asked for with SPECIFIED (ignored otherwise) */
static int cshare_create(struct tsg *t, struct cshare *cs, uint32_t flags, uint32_t subctx, const char *label)
{
    char w[320];
    NV_CTXSHARE_ALLOCATION_PARAMETERS cp = {
        .hVASpace = D.vas, .flags = flags,
        .subctxId = flags == NV_CTXSHARE_ALLOCATION_FLAGS_SUBCONTEXT_SPECIFIED ? subctx : 0xffffffffu
    };
    int r = crm_alloc(D.c, t->h, &cs->h, FERMI_CONTEXT_SHARE_A, &cp, sizeof cp);
    /* bit 31 is the PREFER_LOWER success flag (nvos.h NV_CTXSHARE_ALLOCATION_SUBCTXID_*) */
    cs->veid = cp.subctxId & 0x7fffffffu;
    if (flags == NV_CTXSHARE_ALLOCATION_FLAGS_SUBCONTEXT_SPECIFIED)
        snprintf(w, sizeof w, "%s: alloc FERMI_CONTEXT_SHARE_A (SPECIFIED %u) -> subctxId (VEID) %u", label, subctx,
                 cs->veid);
    else
        snprintf(w, sizeof w, "%s: alloc FERMI_CONTEXT_SHARE_A (%s) -> subctxId (VEID) %u%s", label, cs_name(flags),
                 cs->veid,
                 flags == NV_CTXSHARE_ALLOCATION_FLAGS_SUBCONTEXT_ASYNC_PREFER_LOWER
                     ? ((cp.subctxId >> 31) ? " (lower)" : " (not lower)")
                     : "");
    return say(r, w);
}

static void chan_free_mem(struct chan *ch)
{
    if (ch->ctl_map)
        crm_unmap_memory(D.c, D.dev, ch->ctl, ch->ctl_map, CTL_SIZE, 0);
    if (ch->ctl)
        crm_free(D.c, D.dev, ch->ctl);
    osdesc_free(&ch->ring);
    ch->ctl_map = NULL;
    ch->ctl = 0;
}

/* The channel's memory, then the channel under the TSG and ctxshare, BIND, the
 * objects in obj_mask, the token.  Does not schedule.  Returns 0, or the first
 * failing RM status (printed); *refused names the failing step. */
static int chan_create(struct chan *ch, struct tsg *t, struct cshare *cs, const char **refused)
{
    char w[320];
    int r;
    *refused = NULL;

    snprintf(w, sizeof w, "%s ring", ch->label);
    if (osdesc_alloc(&ch->ring, RING_SIZE, 0, w)) {
        *refused = "ring memory";
        return -1;
    }
    if (ch->ring.va + RING_SIZE > (1ull << 40)) {
        *refused = "ring VA above 2^40";
        return -ERANGE;
    }
    snprintf(w, sizeof w, "%s error notifier + USERD", ch->label);
    if (sysmem_alloc(&ch->ctl, CTL_SIZE, w)) {
        ch->ctl = 0;
        *refused = "control memory";
        return -1;
    }
    r = crm_map_memory(D.c, D.dev, ch->ctl, 0, CTL_SIZE, 0, &ch->ctl_map);
    if (say(r, "CPU-map error notifier + USERD")) {
        ch->ctl_map = NULL;
        *refused = "control map";
        return r;
    }
    memset(ch->ctl_map, 0, CTL_SIZE);
    ch->notifier = ch->ctl_map;
    ch->userd = (volatile uint32_t *)((uint8_t *)ch->ctl_map + CTL_USERD_OFFSET);

    NV_CHANNEL_ALLOC_PARAMS chp;
    memset(&chp, 0, sizeof chp);
    chp.hObjectError = ch->ctl;
    chp.gpFifoOffset = ch->ring.va;
    chp.gpFifoEntries = GPFIFO_ENTRIES;
    chp.hContextShare = cs->h;
    chp.hUserdMemory[0] = ch->ctl;
    chp.userdOffset[0] = CTL_USERD_OFFSET;
    chp.engineType = NV2080_ENGINE_TYPE_GRAPHICS;
    chp.flags = ch->chflags;
    snprintf(w, sizeof w, "%s: alloc BLACKWELL_CHANNEL_GPFIFO_B 0x%x {GR, ctxshare VEID %u, flags 0x%08x}", ch->label,
             CLS_GPFIFO, cs->veid, ch->chflags);
    r = say(crm_alloc(D.c, t->h, &ch->ch, CLS_GPFIFO, &chp, sizeof chp), w);
    if (r) {
        ch->ch = 0;
        *refused = "channel alloc";
        return r;
    }
    uint32_t bind = NV2080_ENGINE_TYPE_GRAPHICS;
    snprintf(w, sizeof w, "%s: NVA06F_CTRL_CMD_BIND {GR}", ch->label);
    r = say(crm_control(D.c, ch->ch, NVA06F_CTRL_CMD_BIND, &bind, sizeof bind), w);
    if (r) {
        *refused = "bind";
        return r;
    }

    static const struct {
        uint32_t bit, cls;
        const char *name;
    } objs[3] = { { OBJ_3D, CLS_3D, "3D" }, { OBJ_COMPUTE, CLS_COMPUTE, "compute" }, { OBJ_COPY, CLS_COPY, "copy" } };
    unsigned no = 0;
    for (unsigned i = 0; i < 3; i++) {
        if (!(ch->obj_mask & objs[i].bit))
            continue;
        snprintf(w, sizeof w, "%s: alloc %s object 0x%x (NULL params)", ch->label, objs[i].name, objs[i].cls);
        r = say(crm_alloc(D.c, ch->ch, &ch->objs[no], objs[i].cls, NULL, 0), w);
        if (r) {
            *refused = objs[i].name;
            return r;
        }
        no++;
    }

    uint32_t idx = NV_CHANNELGPFIFO_NOTIFICATION_TYPE__SIZE_1;
    r = say(crm_control(D.c, ch->ch, NVC36F_CTRL_CMD_GPFIFO_SET_WORK_SUBMIT_TOKEN_NOTIF_INDEX, &idx, sizeof idx),
            "SET_WORK_SUBMIT_TOKEN_NOTIF_INDEX");
    if (r) {
        *refused = "token index";
        return r;
    }
    uint32_t tok = 0;
    r = say(crm_control(D.c, ch->ch, NVC36F_CTRL_CMD_GPFIFO_GET_WORK_SUBMIT_TOKEN, &tok, sizeof tok),
            "GET_WORK_SUBMIT_TOKEN");
    if (r) {
        *refused = "token";
        return r;
    }
    ch->token = tok;
    ch->created = 1;
    printf("       %s: token 0x%08x -> runlist %u, channel id %u, TSG 0x%08x, VEID %u\n", ch->label, tok,
           (tok >> 16) & 0x7fu, tok & 0xfffu, t->h, cs->veid);
    return 0;
}

static int tsg_schedule(struct tsg *t, const char *label)
{
    char w[320];
    NVA06C_CTRL_GPFIFO_SCHEDULE_PARAMS sp = { .bEnable = 1 };
    snprintf(w, sizeof w, "%s: NVA06C_CTRL_CMD_GPFIFO_SCHEDULE {enable}", label);
    int r = say(crm_control(D.c, t->h, NVA06C_CTRL_CMD_GPFIFO_SCHEDULE, &sp, sizeof sp), w);
    if (!r)
        t->scheduled = 1;
    return r;
}

static void tsg_destroy(struct tsg *t, const char *label)
{
    char w[320];
    if (!t->h)
        return;
    if (t->scheduled) {
        NVA06C_CTRL_GPFIFO_SCHEDULE_PARAMS sp = { .bEnable = 0 };
        crm_control(D.c, t->h, NVA06C_CTRL_CMD_GPFIFO_SCHEDULE, &sp, sizeof sp);
    }
    snprintf(w, sizeof w, "%s: free the channel group (ctxshares, channels, objects)", label);
    say(crm_free(D.c, D.dev, t->h), w);
    memset(t, 0, sizeof *t);
}

/* ---- shared memory of a run ---- */

#define COPY_BYTES (1024u * 1024u)
#define COPY_PITCH 4096u

static struct {
    struct gmem stamps, work, out[MAX_CH], copy[2];
} S;

static volatile uint8_t *stamp_cpu(uint32_t off) { return (volatile uint8_t *)S.stamps.cpu + off; }
static uint64_t stamp_va(uint32_t off) { return S.stamps.va + off; }

static int shared_create(void)
{
    if (osdesc_alloc(&S.stamps, PAGE_4K, 0, "stamps (gate, timestamps)"))
        return -1;
    if (osdesc_alloc(&S.work, WORK_SIZE, 0, "work (code, constant buffer 0, QMDs)"))
        return -1;
    for (unsigned i = 0; i < MAX_CH; i++) {
        char w[64];
        snprintf(w, sizeof w, "output %u at the shader's literal VA", i);
        if (osdesc_alloc(&S.out[i], OUT_SIZE, SPIN_OUT_VA_BASE + i * SPIN_OUT_VA_STRIDE, w))
            return -1;
    }
    if (osdesc_alloc(&S.copy[0], COPY_BYTES, 0, "copy source") || osdesc_alloc(&S.copy[1], COPY_BYTES, 0, "copy destination"))
        return -1;
    /* code: variant v at WORK_CODE(v), zero padded to the page */
    memcpy((uint8_t *)S.work.cpu + WORK_CODE(0), spin_code_v0, sizeof spin_code_v0);
    memcpy((uint8_t *)S.work.cpu + WORK_CODE(1), spin_code_v1, sizeof spin_code_v1);
    memcpy((uint8_t *)S.work.cpu + WORK_CODE(2), spin_code_v2, sizeof spin_code_v2);
    return 0;
}

static void shared_destroy(void)
{
    osdesc_free(&S.copy[1]);
    osdesc_free(&S.copy[0]);
    for (unsigned i = 0; i < MAX_CH; i++)
        osdesc_free(&S.out[i]);
    osdesc_free(&S.work);
    osdesc_free(&S.stamps);
}

/* ---- QMD V05_00 (nak/qmd.rs Qmd5_0, fill_qmd) ---- */

#define BITHI(r) (1 ? r)
#define BITLO(r) (0 ? r)
static void qset(uint32_t *q, unsigned hi, unsigned lo, uint64_t v)
{
    for (unsigned b = lo; b <= hi; b++) {
        const uint32_t m = 1u << (b % 32);
        if ((v >> (b - lo)) & 1)
            q[b / 32] |= m;
        else
            q[b / 32] &= ~m;
    }
}
#define QSET(q, f, v) qset((q), BITHI(NVCDC0_QMDV05_00_##f), BITLO(NVCDC0_QMDV05_00_##f), (v))
#define QSETI(q, f, i, v) qset((q), BITHI(NVCDC0_QMDV05_00_##f(i)), BITLO(NVCDC0_QMDV05_00_##f(i)), (v))

static void qmd_build(uint32_t *q, uint64_t prog_va, uint64_t cb_va, unsigned gprs, uint32_t wgs)
{
    memset(q, 0, 96 * 4);
    QSET(q, QMD_MAJOR_VERSION, 5);
    QSET(q, QMD_MINOR_VERSION, 0);
    QSET(q, API_VISIBLE_CALL_LIMIT, NVCDC0_QMDV05_00_API_VISIBLE_CALL_LIMIT_NO_CHECK);
    QSET(q, SAMPLER_INDEX, NVCDC0_QMDV05_00_SAMPLER_INDEX_INDEPENDENTLY);
    QSET(q, QMD_TYPE, 0x2);
    QSET(q, QMD_GROUP_ID, 0x1f);
    QSET(q, BARRIER_COUNT, 0);
    QSET(q, GRID_WIDTH, wgs);
    QSET(q, GRID_HEIGHT, 1);
    QSET(q, GRID_DEPTH, 1);
    QSET(q, CTA_THREAD_DIMENSION0, 32);
    QSET(q, CTA_THREAD_DIMENSION1, 1);
    QSET(q, CTA_THREAD_DIMENSION2, 1);
    QSET(q, PROGRAM_ADDRESS_LOWER_SHIFTED4, (prog_va >> 4) & 0xffffffffu);
    QSET(q, PROGRAM_ADDRESS_UPPER_SHIFTED4, prog_va >> 36);
    QSET(q, REGISTER_COUNT, gprs);
    QSET(q, SHADER_LOCAL_MEMORY_HIGH_SIZE_SHIFTED4, 0);
    QSET(q, SHADER_LOCAL_MEMORY_LOW_SIZE_SHIFTED4, 0);
    QSET(q, DEPENDENCE_COUNTER, 0);
    QSET(q, HW_ONLY_DEPENDENCE_COUNTER, 0);
    /* no shared memory on sm120: sizes 0,8,16,32,64,100 KB -> hw (kb / 4) + 1 */
    QSET(q, SHARED_MEMORY_SIZE_SHIFTED7, 0);
    QSET(q, MIN_SM_CONFIG_SHARED_MEM_SIZE, 1);
    QSET(q, TARGET_SM_CONFIG_SHARED_MEM_SIZE, 1);
    QSET(q, MAX_SM_CONFIG_SHARED_MEM_SIZE, 26);
    /* constant buffer 0: 4 KiB of zeros the shader never reads */
    QSETI(q, CONSTANT_BUFFER_ADDR_LOWER_SHIFTED6, 0, (cb_va >> 6) & 0xffffffffu);
    QSETI(q, CONSTANT_BUFFER_ADDR_UPPER_SHIFTED6, 0, cb_va >> 38);
    QSETI(q, CONSTANT_BUFFER_SIZE_SHIFTED4, 0, 4096 >> 4);
    QSETI(q, CONSTANT_BUFFER_VALID, 0, 1);
}

/* ---- pushes ---- */

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

/* 16-byte release: payload (32 bit) and the GPU timer in the second half */
static unsigned emit_timestamp(uint32_t *p, unsigned n, uint64_t va, uint32_t payload)
{
    return emit_host_sem(p, n, va, payload, SEM_OP_RELEASE | SEM_RELEASE_WFI | SEM_RELEASE_TIMESTAMP);
}

static unsigned emit_compute_setup(uint32_t *p, unsigned n)
{
    p[n++] = mthd(SUBC_COMPUTE, CMP_SET_OBJECT, 1);
    p[n++] = CLS_COMPUTE; /* class_id 15:0, engine_id 20:16 = 0 */
    /* nvk_push_dispatch_state_init, Hopper and later */
    p[n++] = mthd(SUBC_COMPUTE, CMP_SET_SHADER_SHARED_MEMORY_WINDOW_A, 2);
    p[n++] = 1; /* 1 << 32: 4 GiB aligned */
    p[n++] = 0;
    p[n++] = mthd(SUBC_COMPUTE, CMP_SET_SHADER_LOCAL_MEMORY_WINDOW_A, 2);
    p[n++] = 0;
    p[n++] = 0xff000000u;
    p[n++] = mthd(SUBC_COMPUTE, CMP_INVALIDATE_SKED_CACHES, 1); /* the two-word form of P_IMMD */
    p[n++] = 0;
    return n;
}

static unsigned emit_dispatch(uint32_t *p, unsigned n, uint64_t qmd_va)
{
    p[n++] = mthd(SUBC_COMPUTE, CMP_SEND_PCAS_A, 1);
    p[n++] = (uint32_t)(qmd_va >> 8);
    p[n++] = mthd(SUBC_COMPUTE, CMP_SEND_SIGNALING_PCAS2_B, 1);
    p[n++] = CMP_PCAS_ACTION_INVALIDATE_COPY_SCHEDULE;
    return n;
}

static uint32_t *slot(struct chan *ch)
{
    return (uint32_t *)((uint8_t *)ch->ring.cpu + RING_PUSH_OFFSET) + ch->put * PUSH_SLOT_DW;
}

static void kick(struct chan *ch, unsigned dw)
{
    const uint64_t addr = ch->ring.va + RING_PUSH_OFFSET + (uint64_t)ch->put * PUSH_SLOT_B;
    volatile uint64_t *gp = (volatile uint64_t *)ch->ring.cpu;
    gp[ch->put] = ((uint64_t)(((uint32_t)(addr >> 32) & 0xffu) | ((dw & 0x1fffffu) << 10)) << 32) |
                  ((uint32_t)addr & ~3u);
    ch->put = (ch->put + 1) % GPFIFO_ENTRIES;
    full_fence();
    ch->userd[USERD_GP_PUT / 4] = ch->put;
    full_fence();
    *(volatile uint32_t *)((uint8_t *)D.um_map + USERMODE_NOTIFY_CHANNEL_PENDING) = ch->token;
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

/* ---- running ---- */

struct trial {
    double t0[MAX_CH], t1[MAX_CH]; /* GPU timer ns */
    int ok;
};

static uint32_t seq_counter = 100;
static uint64_t gate_value;

static volatile uint64_t *gate_cpu(void) { return (volatile uint64_t *)stamp_cpu(ST_GATE); }

/* Poll a 16-byte timestamp slot until its payload is seq, then read the timer */
static int wait_slot(uint32_t off, uint32_t seq, double deadline, double *ts_ns)
{
    volatile uint32_t *pay = (volatile uint32_t *)stamp_cpu(off);
    volatile uint64_t *ts = (volatile uint64_t *)(stamp_cpu(off) + 8);
    while (*pay != seq) {
        if (now_us() > deadline)
            return 0;
    }
    while (*ts == 0) {
        if (now_us() > deadline)
            return 0;
    }
    *ts_ns = (double)*ts;
    return 1;
}

/* A pitch copy of COPY_BYTES on the channel's copy object (what vkCmdCopyBuffer is in NVK), checked
 * byte for byte.  0 = ok */
static int copy_check(struct chan *ch)
{
    const double tmo = (double)timeout_ms * 1e3;
    uint32_t *src = (uint32_t *)S.copy[0].cpu, *dst = (uint32_t *)S.copy[1].cpu;
    for (size_t i = 0; i < COPY_BYTES / 4; i++) {
        src[i] = (uint32_t)(i * 2654435761u) ^ 0xa5a5c0deu;
        dst[i] = 0;
    }
    full_fence();
    const uint32_t seq = ++seq_counter;
    memset((void *)stamp_cpu(ST_CH(MAX_CH + 2)), 0, 16);
    uint32_t *p = slot(ch);
    unsigned n = 0;
    p[n++] = mthd(SUBC_CE, CE_SET_OBJECT, 1);
    p[n++] = CLS_COPY;
    p[n++] = mthd(SUBC_CE, CE_OFFSET_IN_UPPER, 8);
    p[n++] = (uint32_t)(S.copy[0].va >> 32) & 0x1ffffffu;
    p[n++] = (uint32_t)S.copy[0].va;
    p[n++] = (uint32_t)(S.copy[1].va >> 32) & 0x1ffffffu;
    p[n++] = (uint32_t)S.copy[1].va;
    p[n++] = COPY_PITCH;               /* PITCH_IN */
    p[n++] = COPY_PITCH;               /* PITCH_OUT */
    p[n++] = COPY_PITCH;               /* LINE_LENGTH_IN */
    p[n++] = COPY_BYTES / COPY_PITCH;  /* LINE_COUNT */
    p[n++] = mthd(SUBC_CE, CE_LAUNCH_DMA, 1);
    p[n++] = CE_LAUNCH_NON_PIPELINED | CE_LAUNCH_FLUSH_ENABLE | CE_LAUNCH_SRC_PITCH | CE_LAUNCH_DST_PITCH |
             CE_LAUNCH_MULTI_LINE;
    n = emit_timestamp(p, n, stamp_va(ST_CH(MAX_CH + 2)), seq);
    kick(ch, n);
    double ts;
    if (!wait_slot(ST_CH(MAX_CH + 2), seq, now_us() + tmo, &ts)) {
        channel_error(ch);
        return -ETIMEDOUT;
    }
    if (channel_error(ch))
        return -EIO;
    size_t bad = 0;
    for (size_t i = 0; i < COPY_BYTES / 4; i++)
        bad += dst[i] != src[i];
    printf("       copy on %s: %u KiB through the CE object, %zu bad words\n", ch->label, COPY_BYTES / 1024u, bad);
    return bad ? -EIO : 0;
}

/* Which channels take part in a trial, and the optional dependency */
struct run_cfg {
    unsigned n;
    struct chan *ch[MAX_CH];
    unsigned wgs, k;
    int x_dep;     /* channel 1 acquires X (released by channel 0 after its work) */
    int x_switch;  /* ... with ACQUIRE_SWITCH_TSG */
    int clear_out;
};

static unsigned out_ok_count(unsigned c, unsigned wgs)
{
    volatile uint32_t *o = (volatile uint32_t *)S.out[c].cpu;
    unsigned ok = 0;
    for (unsigned w = 0; w < wgs; w++)
        if (o[2 * w + 1] == 0xc0de0001u && o[2 * w] >= 1200000u)
            ok++;
    return ok;
}

/* Build and kick one trial; the gate opens after every kick.  Fills t.  0 = ok */
static int run_trial(const struct run_cfg *cfg, struct trial *t, unsigned *bad_out, int why_print)
{
    const double tmo = (double)timeout_ms * 1e3;
    uint32_t seq[MAX_CH];
    const uint64_t gate = ++gate_value;
    unsigned variant_gprs[3] = { SPIN_GPRS_V0, SPIN_GPRS_V1, SPIN_GPRS_V2 };
    memset(t, 0, sizeof *t);

    for (unsigned c = 0; c < cfg->n; c++) {
        if (cfg->clear_out)
            memset(S.out[c].cpu, 0, OUT_SIZE);
        seq[c] = ++seq_counter;
        volatile uint8_t *st = stamp_cpu(ST_CH(c));
        memset((void *)st, 0, 64);

        struct chan *ch = cfg->ch[c];
        uint32_t *p = slot(ch);
        unsigned n = 0;
        n = emit_compute_setup(p, n);
        n = emit_host_sem(p, n, stamp_va(ST_GATE), gate, SEM_OP_ACQ_STRICT_GEQ | SEM_PAYLOAD_64);
        if (cfg->x_dep && c == 1)
            n = emit_host_sem(p, n, stamp_va(ST_CH(0) + ST_X), seq[0],
                              SEM_OP_ACQ_STRICT_GEQ | SEM_PAYLOAD_64 | (cfg->x_switch ? SEM_SWITCH_TSG : 0));
        n = emit_timestamp(p, n, stamp_va(ST_CH(c) + ST_T0), seq[c]);
        for (unsigned k = 0; k < cfg->k; k++) {
            const uint64_t qva = S.work.va + WORK_QMD(c, k);
            qmd_build((uint32_t *)((uint8_t *)S.work.cpu + WORK_QMD(c, k)), S.work.va + WORK_CODE(c), S.work.va + WORK_CB0,
                      variant_gprs[c], cfg->wgs);
            n = emit_dispatch(p, n, qva);
        }
        if (cfg->x_dep && c == 0)
            n = emit_host_sem(p, n, stamp_va(ST_CH(0) + ST_X), seq[0],
                              SEM_OP_RELEASE | SEM_RELEASE_WFI | SEM_PAYLOAD_64);
        n = emit_timestamp(p, n, stamp_va(ST_CH(c) + ST_T1), seq[c]);
        if (n > PUSH_SLOT_DW) {
            printf("[FAIL] push of %u dwords does not fit a slot\n", n);
            return -1;
        }
        kick(ch, n);
    }
    full_fence();
    sleep_ms(3); /* every channel is parked on the gate by now */
    *gate_cpu() = gate;
    full_fence();

    const double deadline = now_us() + tmo;
    int fail = 0;
    for (unsigned c = 0; c < cfg->n && !fail; c++) {
        if (!wait_slot(ST_CH(c) + ST_T1, seq[c], deadline, &t->t1[c])) {
            fail = 1;
            if (why_print)
                printf("[FAIL] %s: no completion within %u ms (gate %" PRIu64 ")\n", cfg->ch[c]->label, timeout_ms,
                       gate);
        }
    }
    if (fail) {
        *gate_cpu() = gate + 1000000; /* release anything parked on it */
        gate_value += 1000000;
        for (unsigned c = 0; c < cfg->n; c++)
            channel_error(cfg->ch[c]);
        return -1;
    }
    unsigned bad = 0;
    for (unsigned c = 0; c < cfg->n; c++) {
        double ts;
        if (!wait_slot(ST_CH(c) + ST_T0, seq[c], now_us() + tmo, &ts)) {
            printf("[FAIL] %s: T0 slot never written\n", cfg->ch[c]->label);
            return -1;
        }
        t->t0[c] = ts;
        if (out_ok_count(c, cfg->wgs) != cfg->wgs)
            bad++;
        if (channel_error(cfg->ch[c]))
            return -1;
    }
    if (bad_out)
        *bad_out = bad;
    t->ok = 1;
    return 0;
}

static double dur_us(const struct trial *t, unsigned c) { return (t->t1[c] - t->t0[c]) / 1e3; }

struct overlap {
    double makespan_us, skew_us;
};

/* Gate to last completion: every T0 is taken right after the channel passes the
 * gate (before its kernels), every T1 after its last kernel finished */
static struct overlap overlap_of(const struct trial *t, unsigned n)
{
    double begin = t->t0[0], hi0 = t->t0[0], end = t->t1[0];
    for (unsigned c = 1; c < n; c++) {
        if (t->t0[c] < begin)
            begin = t->t0[c];
        if (t->t0[c] > hi0)
            hi0 = t->t0[c];
        if (t->t1[c] > end)
            end = t->t1[c];
    }
    struct overlap o = { (end - begin) / 1e3, (hi0 - begin) / 1e3 };
    return o;
}

/* ---- scenarios ---- */

struct chdef {
    int tsg, cs;
    uint32_t objs;
    int late;
    int try_copy_after; /* try a copy object after creation (non-fatal) */
    uint32_t chflags;   /* NV_CHANNEL_ALLOC_PARAMS.flags */
};

struct scn {
    const char *name, *desc;
    unsigned ntsg, ncs;
    int cs_tsg[4];
    uint32_t cs_flags[4];
    uint32_t cs_subctx[4]; /* VEID asked for with SPECIFIED */
    unsigned nch;
    struct chdef ch[MAX_CH];
    int run; /* run work (0: creation only) */
};

#define ALL3 (OBJ_3D | OBJ_COMPUTE | OBJ_COPY)
#define CS_SYNC NV_CTXSHARE_ALLOCATION_FLAGS_SUBCONTEXT_SYNC
#define CS_ASYNC NV_CTXSHARE_ALLOCATION_FLAGS_SUBCONTEXT_ASYNC
#define CS_SPEC NV_CTXSHARE_ALLOCATION_FLAGS_SUBCONTEXT_SPECIFIED
#define CS_LOWER NV_CTXSHARE_ALLOCATION_FLAGS_SUBCONTEXT_ASYNC_PREFER_LOWER
/* async* follow what NVIDIA's Vulkan driver does for a compute queue (rmtrace): the compute channel
 * sits in the graphics queue's TSG on a SPECIFIED low VEID (1, 2, ...) and is allocated with
 * RUNQUEUE_ONE | THREAD_ONE.  m-* are creation-only variants that isolate which of the two the
 * compute object needs (each: VEID 0 channel with 3D+compute+copy, then channel B). */
static const struct scn scenarios[] = {
    { "async", "one TSG: SYNC + SPECIFIED VEID 1 (RUNQUEUE_ONE|THREAD_ONE), A = 3D+compute+copy, B = compute (+ copy tried)",
      1, 2, { 0, 0 }, { CS_SYNC, CS_SPEC }, { 0, 1 }, 2,
      { { 0, 0, ALL3, 0, 0, 0 }, { 0, 1, OBJ_COMPUTE, 0, 1, CHF_ASYNC } }, 1 },
    { "async-late", "as async, B created after the TSG was scheduled", 1, 2, { 0, 0 }, { CS_SYNC, CS_SPEC }, { 0, 1 }, 2,
      { { 0, 0, ALL3, 0, 0, 0 }, { 0, 1, OBJ_COMPUTE, 1, 0, CHF_ASYNC } }, 1 },
    { "async3", "one TSG: SYNC + SPECIFIED VEIDs 1 and 2, 3 channels", 1, 3, { 0, 0, 0 }, { CS_SYNC, CS_SPEC, CS_SPEC },
      { 0, 1, 2 }, 3,
      { { 0, 0, ALL3, 0, 0, 0 }, { 0, 1, OBJ_COMPUTE, 0, 0, CHF_ASYNC }, { 0, 2, OBJ_COMPUTE, 0, 0, CHF_ASYNC } }, 1 },
    { "sync2", "one TSG, one SYNC ctxshare, two channels on it", 1, 1, { 0 }, { CS_SYNC }, { 0 }, 2,
      { { 0, 0, ALL3, 0, 0, 0 }, { 0, 0, OBJ_COMPUTE, 0, 0, 0 } }, 1 },
    { "tsg2", "two TSGs (a second NVK context today): A in one, B in the other", 2, 2, { 0, 1 }, { CS_SYNC, CS_SYNC },
      { 0, 0 }, 2, { { 0, 0, ALL3, 0, 0, 0 }, { 1, 1, OBJ_COMPUTE, 0, 0, 0 } }, 1 },
    { "async3d", "VEID 1 compute channel with a 3D object (graphics only runs on VEID 0)", 1, 2, { 0, 0 },
      { CS_SYNC, CS_SPEC }, { 0, 1 }, 2, { { 0, 0, ALL3, 0, 0, 0 }, { 0, 1, OBJ_3D | OBJ_COMPUTE, 0, 0, CHF_ASYNC } },
      0 },
    { "async-old", "the first probe's layout: ASYNC ctxshare (VEID 63), channel flags 0", 1, 2, { 0, 0 },
      { CS_SYNC, CS_ASYNC }, { 0, 0 }, 2, { { 0, 0, ALL3, 0, 0, 0 }, { 0, 1, OBJ_COMPUTE, 0, 0, 0 } }, 0 },
    { "m-async-chf", "ASYNC ctxshare (VEID 63) + RUNQUEUE_ONE|THREAD_ONE", 1, 2, { 0, 0 }, { CS_SYNC, CS_ASYNC },
      { 0, 0 }, 2, { { 0, 0, ALL3, 0, 0, 0 }, { 0, 1, OBJ_COMPUTE, 0, 0, CHF_ASYNC } }, 0 },
    { "m-spec1-nochf", "SPECIFIED VEID 1, channel flags 0", 1, 2, { 0, 0 }, { CS_SYNC, CS_SPEC }, { 0, 1 }, 2,
      { { 0, 0, ALL3, 0, 0, 0 }, { 0, 1, OBJ_COMPUTE, 0, 0, 0 } }, 0 },
    { "m-spec1-rq1", "SPECIFIED VEID 1, RUNQUEUE_ONE only (THREAD 0)", 1, 2, { 0, 0 }, { CS_SYNC, CS_SPEC }, { 0, 1 },
      2, { { 0, 0, ALL3, 0, 0, 0 }, { 0, 1, OBJ_COMPUTE, 0, 0, NVOS04_FLAGS_GROUP_CHANNEL_RUNQUEUE_ONE } }, 0 },
    { "m-spec1-thr1", "SPECIFIED VEID 1, THREAD_ONE only (runqueue 0: GB202 refuses)", 1, 2, { 0, 0 },
      { CS_SYNC, CS_SPEC }, { 0, 1 }, 2,
      { { 0, 0, ALL3, 0, 0, 0 }, { 0, 1, OBJ_COMPUTE, 0, 0, NVOS04_FLAGS_GROUP_CHANNEL_THREAD_ONE } }, 0 },
    { "m-spec8-chf", "SPECIFIED VEID 8 (first above MAX_LOWER_SUBCONTEXT) + RUNQUEUE_ONE|THREAD_ONE", 1, 2, { 0, 0 },
      { CS_SYNC, CS_SPEC }, { 0, 8 }, 2, { { 0, 0, ALL3, 0, 0, 0 }, { 0, 1, OBJ_COMPUTE, 0, 0, CHF_ASYNC } }, 0 },
    { "m-spec63-chf", "SPECIFIED VEID 63 + RUNQUEUE_ONE|THREAD_ONE", 1, 2, { 0, 0 }, { CS_SYNC, CS_SPEC }, { 0, 63 },
      2, { { 0, 0, ALL3, 0, 0, 0 }, { 0, 1, OBJ_COMPUTE, 0, 0, CHF_ASYNC } }, 0 },
    { "m-lower-chf", "ASYNC_PREFER_LOWER + RUNQUEUE_ONE|THREAD_ONE", 1, 2, { 0, 0 }, { CS_SYNC, CS_LOWER }, { 0, 0 },
      2, { { 0, 0, ALL3, 0, 0, 0 }, { 0, 1, OBJ_COMPUTE, 0, 0, CHF_ASYNC } }, 0 },
    { "m-chf-copy", "SPECIFIED VEID 1 + RUNQUEUE_ONE|THREAD_ONE, then a copy object on B", 1, 2, { 0, 0 },
      { CS_SYNC, CS_SPEC }, { 0, 1 }, 2, { { 0, 0, ALL3, 0, 0, 0 }, { 0, 1, OBJ_COMPUTE, 0, 1, CHF_ASYNC } }, 0 },
    { "m-bonly", "SPECIFIED VEID 1 + RUNQUEUE_ONE|THREAD_ONE as the TSG's only channel (no VEID 0 channel)", 1, 2,
      { 0, 0 }, { CS_SYNC, CS_SPEC }, { 0, 1 }, 1, { { 0, 1, OBJ_COMPUTE, 0, 0, CHF_ASYNC } }, 0 },
};
#define NSCN (sizeof scenarios / sizeof scenarios[0])

struct result {
    const char *name;
    int created, alive, ran;
    char refused[64];
    uint32_t veid[MAX_CH];
    unsigned nch;
    double solo_us[MAX_CH];
    double overlap_ratio, makespan_us, skew_us; /* overlap_ratio = 1 - serial fraction */
    const char *verdict;
    int copy_on_async; /* -1 not tried, 0 refused, 1 accepted */
    int copy_works;    /* -1 not tried, 0 failed, 1 data copied correctly */
    int late_needed_sched;
    int markers_ok;
    double handoff_off_us, handoff_on_us, xmakespan_off_us, xmakespan_on_us;
    int xwait_done, xwait_contended;
};

static unsigned opt_reps = 15, opt_wgs = 16, opt_k = 4;
static int opt_xwait, opt_contend;

static int liveness(struct chan *ch)
{
    const double tmo = (double)timeout_ms * 1e3;
    uint32_t *p = slot(ch);
    unsigned n = 0;
    const uint32_t seq = ++seq_counter;
    memset((void *)stamp_cpu(ST_CH(MAX_CH) + 0), 0, 16);
    n = emit_timestamp(p, n, stamp_va(ST_CH(MAX_CH)), seq);
    kick(ch, n);
    double ts;
    if (!wait_slot(ST_CH(MAX_CH), seq, now_us() + tmo, &ts))
        return -ETIMEDOUT;
    return channel_error(ch) ? -EIO : 0;
}

/* A busy channel in a TSG of its own (another process's work: DWM, a game), kept
 * fed with spin pushes while the dependency test runs, so that a TSG switch has
 * somewhere to go.  Its QMDs are built once: rewriting a QMD the GPU may read
 * would show it a zeroed one. */
#define ST_C_DONE (ST_CH(MAX_CH + 1))
#define CONTENDER_VARIANT 2u

struct contender {
    struct tsg tsg;
    struct cshare cs;
    struct chan ch;
    uint64_t kicked;
    int on;
};

static int contender_start(struct contender *c, const char *label)
{
    char w[320];
    const char *refused = NULL;
    memset(c, 0, sizeof *c);
    snprintf(w, sizeof w, "%s contender tsg", label);
    if (tsg_create(&c->tsg, w))
        return -1;
    snprintf(w, sizeof w, "%s contender cs", label);
    if (cshare_create(&c->tsg, &c->cs, NV_CTXSHARE_ALLOCATION_FLAGS_SUBCONTEXT_SYNC, 0, w))
        return -1;
    snprintf(c->ch.label, sizeof c->ch.label, "%s:C", label);
    c->ch.obj_mask = OBJ_COMPUTE;
    if (chan_create(&c->ch, &c->tsg, &c->cs, &refused))
        return -1;
    if (tsg_schedule(&c->tsg, w))
        return -1;
    *(volatile uint64_t *)stamp_cpu(ST_C_DONE) = 0;
    const unsigned variant_gprs[3] = { SPIN_GPRS_V0, SPIN_GPRS_V1, SPIN_GPRS_V2 };
    for (unsigned k = 0; k < MAX_K; k++)
        qmd_build((uint32_t *)((uint8_t *)S.work.cpu + WORK_QMD(MAX_CH - 1, k)),
                  S.work.va + WORK_CODE(CONTENDER_VARIANT), S.work.va + WORK_CB0, variant_gprs[CONTENDER_VARIANT], 16);
    c->on = 1;
    return 0;
}

static void contender_topup(struct contender *c, unsigned want)
{
    while (c->on && c->kicked - *(volatile uint64_t *)stamp_cpu(ST_C_DONE) < want) {
        uint32_t *p = slot(&c->ch);
        unsigned n = emit_compute_setup(p, 0);
        for (unsigned k = 0; k < MAX_K; k++)
            n = emit_dispatch(p, n, S.work.va + WORK_QMD(MAX_CH - 1, k));
        n = emit_host_sem(p, n, stamp_va(ST_C_DONE), ++c->kicked, SEM_OP_RELEASE | SEM_RELEASE_WFI | SEM_PAYLOAD_64);
        kick(&c->ch, n);
    }
}

static void contender_stop(struct contender *c, const char *label)
{
    char w[320];
    if (c->on) {
        const double deadline = now_us() + (double)timeout_ms * 1e3;
        while (*(volatile uint64_t *)stamp_cpu(ST_C_DONE) < c->kicked && now_us() < deadline)
            sleep_ms(1);
        channel_error(&c->ch);
    }
    snprintf(w, sizeof w, "%s contender tsg", label);
    tsg_destroy(&c->tsg, w);
    chan_free_mem(&c->ch);
    c->on = 0;
}

static void run_scenario(const struct scn *sc, struct result *res)
{
    struct tsg tsg[2];
    struct cshare cs[4];
    struct chan ch[MAX_CH];
    struct contender cont;
    char w[320];
    const char *refused = NULL;
    int r;

    memset(&cont, 0, sizeof cont);
    memset(res, 0, sizeof *res);
    res->name = sc->name;
    res->nch = sc->nch;
    res->copy_on_async = -1;
    res->copy_works = -1;
    res->verdict = "-";
    memset(tsg, 0, sizeof tsg);
    memset(cs, 0, sizeof cs);
    memset(ch, 0, sizeof ch);

    printf("\n=== scenario %s: %s\n", sc->name, sc->desc);
    fflush(stdout);

    for (unsigned i = 0; i < sc->ntsg; i++) {
        snprintf(w, sizeof w, "%s tsg%u", sc->name, i);
        if (tsg_create(&tsg[i], w)) {
            snprintf(res->refused, sizeof res->refused, "TSG alloc");
            goto out;
        }
    }
    for (unsigned i = 0; i < sc->ncs; i++) {
        snprintf(w, sizeof w, "%s cs%u", sc->name, i);
        if (cshare_create(&tsg[sc->cs_tsg[i]], &cs[i], sc->cs_flags[i], sc->cs_subctx[i], w)) {
            snprintf(res->refused, sizeof res->refused, "ctxshare %s", cs_name(sc->cs_flags[i]));
            goto out;
        }
    }
    for (unsigned i = 0; i < sc->nch; i++)
        res->veid[i] = cs[sc->ch[i].cs].veid;

    /* channels that exist before the TSGs are scheduled */
    int tsg_used[2] = { 0, 0 };
    for (unsigned i = 0; i < sc->nch; i++) {
        const struct chdef *d = &sc->ch[i];
        if (d->late)
            continue;
        snprintf(ch[i].label, sizeof ch[i].label, "%s:%c", sc->name, (char)('A' + i));
        ch[i].tsg = d->tsg;
        ch[i].cs = d->cs;
        ch[i].obj_mask = d->objs;
        ch[i].chflags = d->chflags;
        r = chan_create(&ch[i], &tsg[d->tsg], &cs[d->cs], &refused);
        if (r) {
            snprintf(res->refused, sizeof res->refused, "%c: %s", (char)('A' + i), refused ? refused : "?");
            goto out;
        }
        tsg_used[d->tsg] = 1;
    }
    for (unsigned i = 0; i < sc->ntsg; i++) {
        if (!tsg_used[i])
            continue;
        snprintf(w, sizeof w, "%s tsg%u", sc->name, i);
        if (tsg_schedule(&tsg[i], w)) {
            snprintf(res->refused, sizeof res->refused, "TSG schedule");
            goto out;
        }
    }
    /* late channels: after the TSG is scheduled */
    for (unsigned i = 0; i < sc->nch; i++) {
        const struct chdef *d = &sc->ch[i];
        if (!d->late)
            continue;
        snprintf(ch[i].label, sizeof ch[i].label, "%s:%c", sc->name, (char)('A' + i));
        ch[i].tsg = d->tsg;
        ch[i].cs = d->cs;
        ch[i].obj_mask = d->objs;
        ch[i].chflags = d->chflags;
        r = chan_create(&ch[i], &tsg[d->tsg], &cs[d->cs], &refused);
        if (r) {
            snprintf(res->refused, sizeof res->refused, "%c: %s", (char)('A' + i), refused ? refused : "?");
            goto out;
        }
    }
    res->created = 1;

    /* a copy object on the async channel (CE copies in a compute queue) */
    for (unsigned i = 0; i < sc->nch; i++) {
        if (!sc->ch[i].try_copy_after)
            continue;
        uint32_t h = 0;
        snprintf(w, sizeof w, "%s: alloc copy object 0x%x on the ASYNC channel (non-fatal)", ch[i].label, CLS_COPY);
        r = say(crm_alloc(D.c, ch[i].ch, &h, CLS_COPY, NULL, 0), w);
        res->copy_on_async = r == 0;
    }

    if (!sc->run) {
        res->alive = 0;
        goto out;
    }

    /* liveness, per channel; a late channel that is silent gets an explicit schedule */
    res->alive = 1;
    for (unsigned i = 0; i < sc->nch; i++) {
        snprintf(w, sizeof w, "%s alive (release with timestamp)", ch[i].label);
        r = say(liveness(&ch[i]), w);
        if (r && sc->ch[i].late) {
            NVA06C_CTRL_GPFIFO_SCHEDULE_PARAMS sp = { .bEnable = 1 };
            snprintf(w, sizeof w, "%s: NVA06F_CTRL_CMD_GPFIFO_SCHEDULE (explicit, channel)", ch[i].label);
            if (!say(crm_control(D.c, ch[i].ch, NVA06F_CTRL_CMD_GPFIFO_SCHEDULE, &sp, sizeof sp), w)) {
                res->late_needed_sched = 1;
                snprintf(w, sizeof w, "%s alive after the explicit schedule", ch[i].label);
                r = say(liveness(&ch[i]), w);
            }
        }
        if (r) {
            res->alive = 0;
            snprintf(res->refused, sizeof res->refused, "%c not alive", (char)('A' + i));
            goto out;
        }
    }

    /* the copy object on the async channel, if RM took it */
    for (unsigned i = 0; i < sc->nch; i++) {
        if (!sc->ch[i].try_copy_after || res->copy_on_async != 1)
            continue;
        r = copy_check(&ch[i]);
        res->copy_works = r == 0;
        say(r, "CE copy on the ASYNC channel");
    }

    /* solo runs */
    for (unsigned i = 0; i < sc->nch; i++) {
        double v[64];
        unsigned nv = 0, bad_total = 0;
        struct run_cfg cfg = { .n = 1, .wgs = opt_wgs, .k = opt_k, .clear_out = 1 };
        cfg.ch[0] = &ch[i];
        for (unsigned rep = 0; rep < opt_reps && nv < 64; rep++) {
            struct trial t;
            unsigned bad = 0;
            /* the output buffer of channel slot 0 is used for a solo run: the shader of variant 0 */
            if (run_trial(&cfg, &t, &bad, 1)) {
                res->ran = 0;
                snprintf(res->refused, sizeof res->refused, "%c solo run failed", (char)('A' + i));
                goto out;
            }
            bad_total += bad;
            v[nv++] = dur_us(&t, 0);
            if (verbose)
                printf("       solo %c trial %u: %.1f us\n", (char)('A' + i), rep, v[nv - 1]);
        }
        res->solo_us[i] = median(v, nv);
        res->markers_ok = bad_total == 0;
        printf("       solo %c: median %.1f us over %u trials (%u launches of %u workgroups), markers %s\n",
               (char)('A' + i), res->solo_us[i], nv, opt_k, opt_wgs, bad_total ? "MISSING" : "ok");
        if (bad_total) {
            snprintf(res->refused, sizeof res->refused, "%c output markers missing", (char)('A' + i));
            goto out;
        }
    }

    /* together: the makespan against the solo durations.  Fully concurrent channels finish
     * in the longest solo time, fully serialised ones in the sum:
     *   serial fraction = (makespan - longest solo) / (sum of solos - longest solo)  */
    {
        double mk[64], sk[64], frac[64];
        double maxsolo = 0, sumsolo = 0;
        for (unsigned i = 0; i < sc->nch; i++) {
            if (res->solo_us[i] > maxsolo)
                maxsolo = res->solo_us[i];
            sumsolo += res->solo_us[i];
        }
        unsigned nv = 0, bad_total = 0;
        struct run_cfg cfg = { .n = sc->nch, .wgs = opt_wgs, .k = opt_k, .clear_out = 1 };
        for (unsigned i = 0; i < sc->nch; i++)
            cfg.ch[i] = &ch[i];
        for (unsigned rep = 0; rep < opt_reps && nv < 64; rep++) {
            struct trial t;
            unsigned bad = 0;
            if (run_trial(&cfg, &t, &bad, 1)) {
                snprintf(res->refused, sizeof res->refused, "together run failed");
                goto out;
            }
            bad_total += bad;
            const struct overlap o = overlap_of(&t, sc->nch);
            mk[nv] = o.makespan_us;
            sk[nv] = o.skew_us;
            frac[nv] = sumsolo > maxsolo ? (o.makespan_us - maxsolo) / (sumsolo - maxsolo) : 0;
            if (frac[nv] < 0)
                frac[nv] = 0;
            if (frac[nv] > 1)
                frac[nv] = 1;
            if (verbose) {
                printf("       together trial %u: start skew %.1f us, makespan %.1f us, serial fraction %.2f, "
                       "completion:",
                       rep, o.skew_us, o.makespan_us, frac[nv]);
                double b0 = t.t0[0];
                for (unsigned c = 1; c < sc->nch; c++)
                    if (t.t0[c] < b0)
                        b0 = t.t0[c];
                for (unsigned c = 0; c < sc->nch; c++)
                    printf(" %c %.1f us", (char)('A' + c), (t.t1[c] - b0) / 1e3);
                printf("\n");
            }
            nv++;
        }
        res->makespan_us = median(mk, nv);
        res->skew_us = median(sk, nv);
        res->overlap_ratio = 1.0 - median(frac, nv);
        res->ran = bad_total == 0;
        if (res->overlap_ratio >= 0.75)
            res->verdict = "CONCURRENT";
        else if (res->overlap_ratio <= 0.25)
            res->verdict = "SERIAL";
        else
            res->verdict = "PARTIAL";
        printf("       together: median start skew %.1f us, makespan %.1f us (longest solo %.1f us, sum of solos "
               "%.1f us) -> %.0f%% overlapped -> %s; markers %s\n",
               res->skew_us, res->makespan_us, maxsolo, sumsolo, 100.0 * res->overlap_ratio, res->verdict,
               bad_total ? "MISSING" : "ok");
    }

    /* dependency test: B acquires what A releases, with and without SWITCH_TSG; with --contend
     * another TSG keeps the GR runlist busy meanwhile */
    if (opt_xwait && sc->nch >= 2) {
        if (opt_contend) {
            if (sc->nch > 2) {
                printf("       --contend needs a scenario with two channels (the contender takes the third slot)\n");
            } else if (contender_start(&cont, sc->name)) {
                snprintf(res->refused, sizeof res->refused, "contender setup failed");
                goto out;
            }
        }
        for (int sw = 0; sw < 2; sw++) {
            double v[64], mkv[64];
            unsigned nv = 0;
            struct run_cfg cfg = { .n = 2, .wgs = opt_wgs, .k = opt_k, .clear_out = 1, .x_dep = 1, .x_switch = sw };
            cfg.ch[0] = &ch[0];
            cfg.ch[1] = &ch[1];
            for (unsigned rep = 0; rep < opt_reps && nv < 64; rep++) {
                struct trial t;
                contender_topup(&cont, 6);
                if (run_trial(&cfg, &t, NULL, 1)) {
                    snprintf(res->refused, sizeof res->refused, "xwait run failed");
                    goto out;
                }
                v[nv] = (t.t0[1] - t.t1[0]) / 1e3;
                const struct overlap o = overlap_of(&t, 2);
                mkv[nv] = o.makespan_us;
                nv++;
            }
            const double m = median(v, nv), mk = median(mkv, nv);
            if (sw) {
                res->handoff_on_us = m;
                res->xmakespan_on_us = mk;
            } else {
                res->handoff_off_us = m;
                res->xmakespan_off_us = mk;
            }
            printf("       xwait%s (B acquires A's release) %s: hand-off T0(B) - T1(A) median %.1f us, makespan "
                   "median %.1f us\n",
                   cont.on ? " + contender TSG" : "", sw ? "with ACQUIRE_SWITCH_TSG" : "without SWITCH_TSG", m, mk);
        }
        res->xwait_done = 1;
        res->xwait_contended = cont.on;
    }

out:
    /* Quiesce: open the gate for good, then free the TSGs (channels, objects) before the memory */
    *gate_cpu() = ~0ull >> 1;
    gate_value = ~0ull >> 2;
    contender_stop(&cont, sc->name);
    for (unsigned i = 0; i < sc->ntsg; i++) {
        snprintf(w, sizeof w, "%s tsg%u", sc->name, i);
        tsg_destroy(&tsg[i], w);
    }
    for (unsigned i = 0; i < MAX_CH; i++)
        chan_free_mem(&ch[i]);
    *gate_cpu() = 0;
    gate_value = 0;
}

/* ---- main ---- */

static void usage(void)
{
    fprintf(stderr,
            "usage: async_compute_probe [--scenario list] [--reps n] [--wgs n] [--k n] [--xwait] [--contend] [--timeout-ms ms] "
            "[--verbose]\n  scenarios: async,async-late,async3,sync2,tsg2,async3d,async-old,m-async-chf,m-spec1-nochf,\n"
            "             m-spec1-rq1,m-spec1-thr1,m-spec8-chf,m-spec63-chf,m-lower-chf,m-chf-copy,m-bonly,\n"
            "             matrix (= async-old,async3d and every m-*: creation only, no GPU work), all\n");
}

int main(int argc, char **argv)
{
    unsigned sel = 0;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--scenario") && i + 1 < argc) {
            char *list = strdup(argv[++i]);
            for (char *tok = strtok(list, ","); tok; tok = strtok(NULL, ",")) {
                if (!strcmp(tok, "all")) {
                    sel = (1u << NSCN) - 1;
                    continue;
                }
                if (!strcmp(tok, "matrix")) {
                    for (unsigned j = 0; j < NSCN; j++)
                        if (!scenarios[j].run)
                            sel |= 1u << j;
                    continue;
                }
                unsigned j;
                for (j = 0; j < NSCN; j++)
                    if (!strcmp(tok, scenarios[j].name)) {
                        sel |= 1u << j;
                        break;
                    }
                if (j == NSCN) {
                    fprintf(stderr, "unknown scenario %s\n", tok);
                    usage();
                    return 2;
                }
            }
            free(list);
        } else if (!strcmp(argv[i], "--reps") && i + 1 < argc) {
            opt_reps = (unsigned)strtoul(argv[++i], NULL, 0);
        } else if (!strcmp(argv[i], "--wgs") && i + 1 < argc) {
            opt_wgs = (unsigned)strtoul(argv[++i], NULL, 0);
        } else if (!strcmp(argv[i], "--k") && i + 1 < argc) {
            opt_k = (unsigned)strtoul(argv[++i], NULL, 0);
        } else if (!strcmp(argv[i], "--timeout-ms") && i + 1 < argc) {
            timeout_ms = (unsigned)strtoul(argv[++i], NULL, 0);
        } else if (!strcmp(argv[i], "--xwait")) {
            opt_xwait = 1;
        } else if (!strcmp(argv[i], "--contend")) {
            opt_xwait = 1;
            opt_contend = 1;
        } else if (!strcmp(argv[i], "--verbose")) {
            verbose = 1;
        } else {
            usage();
            return 2;
        }
    }
    if (!sel)
        sel = (1u << NSCN) - 1;
    if (opt_reps < 1)
        opt_reps = 1;
    if (opt_reps > 64)
        opt_reps = 64;
    if (opt_k < 1 || opt_k > MAX_K || opt_wgs < 1 || opt_wgs > MAX_WGS) {
        fprintf(stderr, "--k 1..%u, --wgs 1..%u\n", MAX_K, MAX_WGS);
        return 2;
    }
    timer_init();

    printf("async_compute_probe: GB202 classes (gpfifo 0x%x, 3D 0x%x, compute 0x%x, copy 0x%x), spin %u launches x "
           "%u workgroups of 32 threads, %u trials\n",
           CLS_GPFIFO, CLS_3D, CLS_COMPUTE, CLS_COPY, opt_k, opt_wgs, opt_reps);

    struct result results[NSCN];
    unsigned nres = 0;
    int fatal = 0;

    if (say(crm_open(&D.c, NULL), "crm_open")) {
        D.c = NULL;
        return 1;
    }
    printf("       RM version %s, client 0x%08x\n", crm_rm_version(D.c), crm_root(D.c));
    {
        NV0080_ALLOC_PARAMETERS dp;
        memset(&dp, 0, sizeof dp);
        dp.hClientShare = crm_root(D.c);
        dp.flags = NV_DEVICE_ALLOCATION_FLAGS_VASPACE_BIG_PAGE_SIZE_64k;
        dp.vaMode = NV_DEVICE_ALLOCATION_VAMODE_OPTIONAL_MULTIPLE_VASPACES;
        if (say(crm_alloc(D.c, crm_root(D.c), &D.dev, NV01_DEVICE_0, &dp, sizeof dp), "alloc NV01_DEVICE_0")) {
            D.dev = 0;
            goto out;
        }
        NV2080_ALLOC_PARAMETERS sp = { .subDeviceId = 0 };
        if (say(crm_alloc(D.c, D.dev, &D.sub, NV20_SUBDEVICE_0, &sp, sizeof sp), "alloc NV20_SUBDEVICE_0")) {
            D.sub = 0;
            goto out;
        }
        NV_VASPACE_ALLOCATION_PARAMETERS vp;
        memset(&vp, 0, sizeof vp);
        vp.index = NV_VASPACE_ALLOCATION_INDEX_GPU_DEVICE;
        if (say(crm_alloc(D.c, D.dev, &D.vas, FERMI_VASPACE_A, &vp, sizeof vp),
                "alloc FERMI_VASPACE_A (the device's VA space)")) {
            D.vas = 0;
            goto out;
        }
    }
    {
        NV2080_CTRL_FIFO_GET_INFO_PARAMS *fp = calloc(1, sizeof *fp);
        if (fp) {
            fp->fifoInfoTblSize = 2;
            fp->fifoInfoTbl[0].index = NV2080_CTRL_FIFO_INFO_INDEX_MAX_SUBCONTEXT_PER_GROUP;
            fp->fifoInfoTbl[1].index = NV2080_CTRL_FIFO_INFO_INDEX_MAX_LOWER_SUBCONTEXT;
            fp->engineType = NV2080_ENGINE_TYPE_GRAPHICS;
            int r = crm_control(D.c, D.sub, NV2080_CTRL_CMD_FIFO_GET_INFO, fp, sizeof *fp);
            if (r == 0)
                printf("       FIFO_GET_INFO: max subcontexts per group %u, max lower subcontext %u\n",
                       fp->fifoInfoTbl[0].data, fp->fifoInfoTbl[1].data);
            else
                printf("       FIFO_GET_INFO 0x%x: %s (informational)\n", NV2080_CTRL_CMD_FIFO_GET_INFO,
                       crm_status_name(r));
            free(fp);
        }
    }
    {
        NV_HOPPER_USERMODE_A_PARAMS up = { .bBar1Mapping = 1, .bPriv = 0 };
        if (say(crm_alloc(D.c, D.sub, &D.um, CLS_USERMODE, &up, sizeof up), "alloc BLACKWELL_USERMODE_A (doorbell)")) {
            D.um = 0;
            goto out;
        }
        if (say(crm_map_memory(D.c, D.sub, D.um, 0, USERMODE_SIZE, 0, &D.um_map), "CPU-map the doorbell")) {
            D.um_map = NULL;
            goto out;
        }
    }
    if (shared_create()) {
        fatal = 1;
        goto out;
    }
    /* constant buffer 0 stays zero; the stamps page's liveness slot is block MAX_CH */
    if (ST_CH(MAX_CH) + 64 > PAGE_4K) {
        fatal = 1;
        goto out;
    }

    for (unsigned j = 0; j < NSCN; j++) {
        if (!(sel & (1u << j)))
            continue;
        run_scenario(&scenarios[j], &results[nres++]);
    }

    printf("\nsummary (GPU-timer intervals; solo = the channel alone, together = all gated to start at once)\n");
    printf("%-14s %-22s %-8s %-18s %-9s %-8s %-8s %-11s\n", "scenario", "create", "VEIDs", "solo us", "makespan us",
           "overlap", "skew us", "verdict");
    for (unsigned i = 0; i < nres; i++) {
        const struct result *r = &results[i];
        char veids[32] = "", solo[64] = "";
        for (unsigned c = 0; c < r->nch; c++) {
            char b[16];
            snprintf(b, sizeof b, c ? "/%u" : "%u", r->veid[c]);
            strncat(veids, b, sizeof veids - strlen(veids) - 1);
        }
        for (unsigned c = 0; c < r->nch; c++) {
            char b[16];
            snprintf(b, sizeof b, c ? "/%.0f" : "%.0f", r->solo_us[c]);
            strncat(solo, b, sizeof solo - strlen(solo) - 1);
        }
        char cr[64];
        if (r->created)
            snprintf(cr, sizeof cr, "ok");
        else
            snprintf(cr, sizeof cr, "REFUSED %s", r->refused);
        char ov[16];
        snprintf(ov, sizeof ov, "%.0f%%", 100.0 * r->overlap_ratio);
        int conly = 0;
        for (unsigned j = 0; j < NSCN; j++)
            if (!strcmp(scenarios[j].name, r->name))
                conly = !scenarios[j].run;
        printf("%-14s %-22s %-8s %-18s %-9.0f %-8s %-8.1f %-11s\n", r->name, cr, veids, solo, r->makespan_us, ov,
               r->skew_us, r->ran ? r->verdict : (r->created ? (conly ? "created" : "not run/failed") : "-"));
        if (r->copy_on_async >= 0)
            printf("            copy object on the ASYNC channel: %s%s\n", r->copy_on_async ? "accepted" : "refused",
                   r->copy_works < 0 ? "" : (r->copy_works ? ", CE copy verified" : ", CE copy FAILED"));
        if (r->late_needed_sched)
            printf("            late channel needed an explicit NVA06F_CTRL_CMD_GPFIFO_SCHEDULE\n");
        if (r->xwait_done)
            printf("            xwait%s: hand-off %.1f us / makespan %.1f us without SWITCH_TSG, %.1f us / %.1f us with\n",
                   r->xwait_contended ? " (contender TSG busy)" : "", r->handoff_off_us, r->xmakespan_off_us,
                   r->handoff_on_us, r->xmakespan_on_us);
        if (!r->created || (!r->ran && r->refused[0]))
            printf("            note: %s\n", r->refused);
    }

out:
    if (D.c) {
        shared_destroy();
        if (D.um_map)
            crm_unmap_memory(D.c, D.sub, D.um, D.um_map, USERMODE_SIZE, 0);
        if (D.um)
            crm_free(D.c, D.sub, D.um);
        if (D.vas)
            crm_free(D.c, D.dev, D.vas);
        if (D.sub)
            crm_free(D.c, D.dev, D.sub);
        if (D.dev)
            crm_free(D.c, crm_root(D.c), D.dev);
        const size_t objs = crm_object_count(D.c), maps = crm_mapping_count(D.c);
        printf("       objects still tracked %zu, CPU mappings %zu\n", objs, maps);
        crm_close(D.c);
    }
    {
        /* PASS: the async scenario created its channels and both ran with every marker written; if it
         * was not selected, every selected scenario that runs work did that */
        int have_async = 0, async_ok = 0, all_ok = nres > 0;
        for (unsigned i = 0; i < nres; i++) {
            const int ok = results[i].created && results[i].alive && results[i].ran;
            if (!strcmp(results[i].name, "async")) {
                have_async = 1;
                async_ok = ok;
            }
            /* creation-only scenarios (async3d, async-old, m-*) record a status, they run nothing */
            int creation_only = 0;
            for (unsigned j = 0; j < NSCN; j++)
                if (!strcmp(scenarios[j].name, results[i].name))
                    creation_only = !scenarios[j].run;
            if (!creation_only && !ok)
                all_ok = 0;
        }
        const int pass = !fatal && (have_async ? async_ok : all_ok);
        printf("RESULT %s\n", pass ? "PASS" : "FAIL");
        return pass ? 0 : 1;
    }
}
