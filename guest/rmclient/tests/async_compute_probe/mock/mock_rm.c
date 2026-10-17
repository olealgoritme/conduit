/* SPDX-License-Identifier: MIT */
/*
 * mock_rm: a software stand-in for librmclient AND the GPU, so that
 * async_compute_probe's own logic (channel setup order, GPFIFO / USERD /
 * doorbell protocol, gate, timestamps, verdict arithmetic, teardown) can be
 * run on a machine without the VM.  It is a test aid, not a model of the
 * hardware: it implements the RM calls the probe makes, runs each channel's
 * GPFIFO in its own thread (host semaphore acquire/release with the
 * timestamp half, compute launches through SEND_PCAS_A / PCAS2_B with the
 * QMD's grid and program address) and decides how the "GPU" shares:
 *
 *   MOCK_MODEL=async       kernels of different TSGs exclude each other (time
 *                          slicing); inside a TSG kernels of different VEIDs
 *                          overlap and kernels of one VEID are serialised (default)
 *   MOCK_MODEL=serial      every kernel on the GPU is serialised
 *   MOCK_MODEL=concurrent  nothing is ever serialised
 *   MOCK_REFUSE=async-compute|async-3d|async-ctxshare   RM refuses that step
 *   MOCK_SPIN_US=<us>      time of one launch (default 500)
 *
 * Build and run: mock/run-mock.sh
 */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include "rmclient.h"
#include "nv_ioctl_defs.h"
#include "rm_params.h"

#define MAXOBJ 4096
#define MAXMAP 256

enum kind { K_NONE, K_MEM, K_OSDESC, K_TSG, K_CTXSHARE, K_CHANNEL, K_OTHER };

struct obj {
    enum kind kind;
    uint32_t parent, cls;
    void *mem; /* K_MEM: the "RM" memory, K_OSDESC: the caller's pages */
    uint64_t size;
    /* K_CTXSHARE */
    uint32_t veid, tsg;
    /* K_CHANNEL */
    uint32_t h_ctxshare, h_userd_mem;
    uint64_t gpfifo_va, userd_off;
    uint32_t entries, id;
    int is_async_compute;
};

struct vamap {
    uint64_t va, size;
    uint8_t *ptr;
    int used;
};

struct chanrt {
    int used;
    struct obj *o;
    uint32_t handle;
    pthread_t th;
    atomic_int token_pending;
    atomic_int pending_kernels;
    uint32_t tsg, veid;
    uint32_t get;
    int error;
};

static struct obj objs[MAXOBJ];
static struct vamap maps[MAXMAP];
static uint32_t next_handle = 0x5c000001u;
static uint64_t next_auto_va = 0x1000000000ull;
static void *um_map;
static pthread_mutex_t mx = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv = PTHREAD_COND_INITIALIZER;
static atomic_int quit;
static struct chanrt chans[64];
static unsigned nchans;
static uint32_t next_async_veid = 1;
static unsigned live_objs;
static pthread_t db_thread;
static int db_started;
static unsigned mock_spin_us = 500;
static int model; /* 0 async, 1 serial, 2 concurrent */
static const char *refuse = "";

/* engine state */
static uint32_t owner_tsg;
static unsigned owner_users;
static pthread_mutex_t veid_mx[16][4];

static struct obj *H(uint32_t h) { return (h >= 0x5c000001u && h - 0x5c000001u < MAXOBJ) ? &objs[h - 0x5c000001u] : NULL; }

static uint64_t va_base(uint64_t va)
{
    for (unsigned i = 0; i < MAXMAP; i++)
        if (maps[i].used && va >= maps[i].va && va < maps[i].va + maps[i].size)
            return maps[i].va;
    return 0;
}

static uint8_t *va_ptr(uint64_t va)
{
    for (unsigned i = 0; i < MAXMAP; i++)
        if (maps[i].used && va >= maps[i].va && va < maps[i].va + maps[i].size)
            return maps[i].ptr + (va - maps[i].va);
    return NULL;
}

static double ns_now(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec * 1e9 + (double)ts.tv_nsec;
}

/* ---- the "GPU" ---- */

struct kernel_launch {
    struct chanrt *ch;
    uint64_t qmd_va;
};

#define BITS(q, hi, lo) ({ uint64_t v_ = 0; for (int b_ = (hi); b_ >= (int)(lo); b_--) v_ = (v_ << 1) | (((q)[b_ / 32] >> (b_ % 32)) & 1); v_; })

static void *kernel_thread(void *arg)
{
    struct kernel_launch *kl = arg;
    struct chanrt *ch = kl->ch;
    const uint32_t *q = (const uint32_t *)va_ptr(kl->qmd_va);
    /* NVCDC0_QMDV05_00: GRID_WIDTH 1279:1248, PROGRAM_ADDRESS_LOWER_SHIFTED4 1055:1024, UPPER 1076:1056 */
    const uint32_t wgs = (uint32_t)BITS(q, 1279, 1248);
    const uint64_t prog = (BITS(q, 1055, 1024) | (BITS(q, 1076, 1056) << 32)) << 4;
    const uint32_t *cb0 = (const uint32_t *)va_ptr(0); /* unused */
    (void)cb0;

    if (model != 2) {
        pthread_mutex_lock(&mx);
        if (model == 1) {
            while (owner_users)
                pthread_cond_wait(&cv, &mx);
            owner_tsg = ch->tsg;
            owner_users++;
        } else {
            while (owner_users && owner_tsg != ch->tsg)
                pthread_cond_wait(&cv, &mx);
            owner_tsg = ch->tsg;
            owner_users++;
        }
        pthread_mutex_unlock(&mx);
        if (model == 0)
            pthread_mutex_lock(&veid_mx[ch->tsg % 16][ch->veid % 4]);
    }

    usleep(mock_spin_us);

    /* the output: variant = (program address - work base) / 4096, 3 variants at 0x3000000000 + n * 0x400000 */
    uint8_t *code = va_ptr(prog);
    (void)code;
    const uint32_t variant = (uint32_t)((prog - va_base(prog)) / 4096);
    uint32_t *out = (uint32_t *)va_ptr(0x3000000000ull + (uint64_t)variant * 0x400000ull);
    if (out)
        for (uint32_t w = 0; w < wgs; w++) {
            out[2 * w] = 1300000u;
            out[2 * w + 1] = 0xc0de0001u;
        }

    if (model != 2) {
        if (model == 0)
            pthread_mutex_unlock(&veid_mx[ch->tsg % 16][ch->veid % 4]);
        pthread_mutex_lock(&mx);
        owner_users--;
        pthread_cond_broadcast(&cv);
        pthread_mutex_unlock(&mx);
    }
    atomic_fetch_sub(&ch->pending_kernels, 1);
    free(kl);
    return NULL;
}

static void wait_idle(struct chanrt *ch)
{
    while (atomic_load(&ch->pending_kernels) > 0 && !atomic_load(&quit))
        usleep(20);
}

static void run_push(struct chanrt *ch, const uint32_t *p, unsigned dw)
{
    uint32_t sem_lo = 0, sem_hi = 0, pay_lo = 0, pay_hi = 0;
    uint64_t qmd_pcas = 0;
    unsigned i = 0;
    while (i < dw && !atomic_load(&quit)) {
        const uint32_t hdr = p[i++];
        const uint32_t op = hdr >> 29, count = (hdr >> 16) & 0x1fff, subc = (hdr >> 13) & 7, mth = (hdr & 0x1fff) << 2;
        if (op != 1) {
            fprintf(stderr, "mock: unsupported header 0x%08x\n", hdr);
            ch->error = 1;
            return;
        }
        for (uint32_t j = 0; j < count; j++) {
            const uint32_t m = mth + 4 * j, d = p[i + j];
            if (subc == 0) {
                switch (m) {
                case 0x5c: sem_lo = d; break;
                case 0x60: sem_hi = d; break;
                case 0x64: pay_lo = d; break;
                case 0x68: pay_hi = d; break;
                case 0x6c: {
                    const uint64_t va = ((uint64_t)sem_hi << 32) | sem_lo;
                    volatile uint8_t *mem = (volatile uint8_t *)va_ptr(va);
                    const uint32_t opn = d & 7, wfi = (d >> 20) & 1, p64 = (d >> 24) & 1, ts = (d >> 25) & 1;
                    const uint64_t payload = ((uint64_t)pay_hi << 32) | pay_lo;
                    if (!mem) {
                        fprintf(stderr, "mock: semaphore VA 0x%llx unmapped\n", (unsigned long long)va);
                        ch->error = 1;
                        return;
                    }
                    if (opn == 2) { /* ACQ_STRICT_GEQ */
                        while (!atomic_load(&quit)) {
                            const uint64_t v = p64 ? *(volatile uint64_t *)mem : *(volatile uint32_t *)mem;
                            if (v >= payload)
                                break;
                            usleep(10);
                        }
                    } else if (opn == 1) { /* RELEASE */
                        if (wfi)
                            wait_idle(ch);
                        if (ts) {
                            *(volatile uint64_t *)(mem + 8) = (uint64_t)ns_now();
                            __atomic_thread_fence(__ATOMIC_SEQ_CST);
                            *(volatile uint32_t *)mem = (uint32_t)payload;
                        } else if (p64) {
                            *(volatile uint64_t *)mem = payload;
                        } else {
                            *(volatile uint32_t *)mem = (uint32_t)payload;
                        }
                    }
                    break;
                }
                default: break;
                }
            } else if (subc == 4) { /* copy engine: pitch copies */
                static uint32_t ce[8];
                if (m >= 0x400 && m < 0x420)
                    ce[(m - 0x400) / 4] = d;
                else if (m == 0x300) {
                    const uint64_t src = ((uint64_t)ce[0] << 32) | ce[1], dst = ((uint64_t)ce[2] << 32) | ce[3];
                    uint8_t *sp = va_ptr(src), *dp = va_ptr(dst);
                    if (!sp || !dp) {
                        ch->error = 1;
                    } else {
                        for (uint32_t l = 0; l < ce[7]; l++)
                            memcpy(dp + (uint64_t)l * ce[5], sp + (uint64_t)l * ce[4], ce[6]);
                    }
                }
            } else {
                if (m == 0x2b4)
                    qmd_pcas = (uint64_t)d << 8;
                else if (m == 0x2c0 && d == 3) {
                    struct kernel_launch *kl = malloc(sizeof *kl);
                    kl->ch = ch;
                    kl->qmd_va = qmd_pcas;
                    atomic_fetch_add(&ch->pending_kernels, 1);
                    pthread_t t;
                    pthread_create(&t, NULL, kernel_thread, kl);
                    pthread_detach(t);
                }
            }
        }
        i += count;
    }
}

static void *channel_thread(void *arg)
{
    struct chanrt *ch = arg;
    struct obj *o = ch->o;
    uint32_t *ring = (uint32_t *)va_ptr(o->gpfifo_va);
    struct obj *ctl = H(o->h_userd_mem);
    volatile uint32_t *userd = (volatile uint32_t *)((uint8_t *)ctl->mem + o->userd_off);
    while (!atomic_load(&quit) && ch->used) {
        /* the doorbell write is not needed to wake this thread: GP_PUT is watched (a doorbell
         * register that two channels write back to back would lose one token here) */
        {
            const uint32_t put = userd[0x8c / 4];
            while (ch->get != put && !atomic_load(&quit)) {
                const uint64_t e = *(volatile uint64_t *)(ring + 2 * ch->get);
                const uint32_t lo = (uint32_t)e, hi = (uint32_t)(e >> 32);
                const uint64_t addr = ((uint64_t)(hi & 0xff) << 32) | (lo & ~3u);
                const unsigned len = (hi >> 10) & 0x1fffff;
                const uint32_t *pb = (const uint32_t *)va_ptr(addr);
                if (!pb) {
                    fprintf(stderr, "mock: pushbuffer VA 0x%llx unmapped\n", (unsigned long long)addr);
                    ch->error = 1;
                    break;
                }
                run_push(ch, pb, len);
                ch->get = (ch->get + 1) % o->entries;
            }
        }
        usleep(20);
    }
    return NULL;
}

static void *doorbell_thread(void *arg)
{
    (void)arg;
    while (!atomic_load(&quit)) {
        volatile uint32_t *db = (volatile uint32_t *)((uint8_t *)um_map + 0x90);
        const uint32_t tok = *db;
        if (tok) {
            *db = 0;
            for (unsigned i = 0; i < nchans; i++)
                if (chans[i].used && (chans[i].o->id == (tok & 0xfff)))
                    atomic_store(&chans[i].token_pending, 1);
        }
        usleep(5);
    }
    return NULL;
}

/* ---- RM API ---- */

struct crm_client {
    int dummy;
};
static struct crm_client the_client;

int crm_open(crm_client **out, const struct crm_transport *t)
{
    (void)t;
    const char *m = getenv("MOCK_MODEL");
    model = m && !strcmp(m, "serial") ? 1 : (m && !strcmp(m, "concurrent") ? 2 : 0);
    refuse = getenv("MOCK_REFUSE") ? getenv("MOCK_REFUSE") : "";
    if (getenv("MOCK_SPIN_US"))
        mock_spin_us = (unsigned)atoi(getenv("MOCK_SPIN_US"));
    *out = &the_client;
    return 0;
}

uint32_t crm_root(const crm_client *c) { (void)c; return 0xc1d00001u; }
const char *crm_rm_version(const crm_client *c) { (void)c; return "mock"; }
const char *crm_status_name(int s) { return s == 0 ? "NV_OK" : (s > 0 ? "NV_ERR_MOCK_REFUSED" : "-errno"); }
const char *crm_status_string(int s) { (void)s; return "mock"; }
size_t crm_object_count(const crm_client *c) { (void)c; return live_objs; }
size_t crm_mapping_count(const crm_client *c)
{
    (void)c;
    size_t n = 0;
    for (unsigned i = 0; i < MAXMAP; i++)
        n += maps[i].used;
    return n;
}

int crm_alloc(crm_client *c, uint32_t parent, uint32_t *object, uint32_t hclass, void *params, uint32_t size)
{
    (void)c;
    uint32_t h = next_handle++;
    if (h - 0x5c000001u >= MAXOBJ)
        return -ENOMEM;
    struct obj *o = H(h);
    memset(o, 0, sizeof *o);
    o->parent = parent;
    o->cls = hclass;
    o->kind = K_OTHER;
    switch (hclass) {
    case NV01_MEMORY_SYSTEM: {
        NV_MEMORY_ALLOCATION_PARAMS *mp = params;
        o->kind = K_MEM;
        o->size = mp->size;
        o->mem = calloc(1, mp->size);
        break;
    }
    case 0xa06c:
        o->kind = K_TSG;
        break;
    case 0x9067: {
        uint32_t *cp = params; /* hVASpace, flags, subctxId */
        o->kind = K_CTXSHARE;
        o->tsg = parent;
        if (cp[1] == 0) {
            o->veid = 0;
        } else {
            if (!strcmp(refuse, "async-ctxshare")) {
                o->kind = K_NONE;
                *object = 0;
                return 0x1f;
            }
            o->veid = next_async_veid++;
        }
        cp[2] = o->veid;
        break;
    }
    case 0xca6f: {
        NV_CHANNEL_ALLOC_PARAMS *cp = params;
        o->kind = K_CHANNEL;
        o->h_ctxshare = cp->hContextShare;
        o->h_userd_mem = cp->hUserdMemory[0];
        o->userd_off = cp->userdOffset[0];
        o->gpfifo_va = cp->gpFifoOffset;
        o->entries = cp->gpFifoEntries;
        o->id = nchans + 1;
        o->tsg = parent;
        break;
    }
    case 0xce97: { /* 3D */
        struct obj *ch = H(parent);
        struct obj *cs = ch ? H(ch->h_ctxshare) : NULL;
        if (cs && cs->veid != 0) { /* what the README reports for GSP */
            o->kind = K_NONE;
            o->kind = K_NONE;
            *object = 0;
            return 0x1f; /* NV_ERR_INVALID_OBJECT-ish */
        }
        break;
    }
    case 0xcec0: { /* compute */
        struct obj *ch = H(parent);
        struct obj *cs = ch ? H(ch->h_ctxshare) : NULL;
        if (cs && cs->veid != 0 && !strcmp(refuse, "async-compute")) {
            o->kind = K_NONE;
            *object = 0;
            return 0x1f;
        }
        break;
    }
    default:
        break;
    }
    if (hclass == 0xcab5) {
        struct obj *ch = H(parent);
        struct obj *cs = ch ? H(ch->h_ctxshare) : NULL;
        if (cs && cs->veid != 0 && !strcmp(refuse, "async-copy")) {
            o->kind = K_NONE;
            *object = 0;
            return 0x1f; /* copy on async refused, to exercise that path */
        }
    }
    if (hclass == 0xc761)
        o->kind = K_OTHER;
    live_objs++;
    *object = h;
    return 0;
}

int crm_control(crm_client *c, uint32_t object, uint32_t cmd, void *params, uint32_t size)
{
    (void)c; (void)size;
    struct obj *o = H(object);
    switch (cmd) {
    case 0xc36f0108: { /* GET_WORK_SUBMIT_TOKEN */
        *(uint32_t *)params = (22u << 16) | o->id;
        struct obj *ctx = H(o->h_ctxshare);
        struct chanrt *ch = &chans[nchans++];
        memset(ch, 0, sizeof *ch);
        ch->used = 1;
        ch->o = o;
        ch->handle = object;
        ch->tsg = o->tsg;
        ch->veid = ctx ? ctx->veid : 0;
        pthread_create(&ch->th, NULL, channel_thread, ch);
        return 0;
    }
    case 0x20801109: { /* FIFO_GET_INFO */
        uint32_t *p = params; /* tblSize, [index,data]... */
        p[2] = 64;
        p[4] = 10;
        return 0;
    }
    default:
        return 0;
    }
}

int crm_free(crm_client *c, uint32_t parent, uint32_t object)
{
    (void)c; (void)parent;
    struct obj *o = H(object);
    if (!o || o->kind == K_NONE)
        return 0;
    if (o->kind == K_TSG) {
        /* free the channels under it first: stop their threads */
        for (unsigned i = 0; i < nchans; i++)
            if (chans[i].used && chans[i].o->tsg == object) {
                chans[i].used = 0;
                pthread_join(chans[i].th, NULL);
            }
        /* everything below it, recursively: marked live while a parent in the doomed set exists */
        uint8_t doomed[MAXOBJ];
        memset(doomed, 0, sizeof doomed);
        doomed[object - 0x5c000001u] = 1;
        for (int changed = 1; changed;) {
            changed = 0;
            for (unsigned i = 0; i < MAXOBJ; i++) {
                if (objs[i].kind == K_NONE || doomed[i] || objs[i].parent < 0x5c000001u)
                    continue;
                const uint32_t pi = objs[i].parent - 0x5c000001u;
                if (pi < MAXOBJ && doomed[pi]) {
                    doomed[i] = 1;
                    changed = 1;
                }
            }
        }
        for (unsigned i = 0; i < MAXOBJ; i++)
            if (doomed[i] && &objs[i] != o && objs[i].kind != K_NONE) {
                objs[i].kind = K_NONE;
                live_objs--;
            }
    }
    if (o->kind == K_MEM)
        free(o->mem);
    o->kind = K_NONE;
    live_objs--;
    return 0;
}

int crm_map_memory(crm_client *c, uint32_t device, uint32_t memory, uint64_t offset, uint64_t length, uint32_t flags, void **cpu_ptr)
{
    (void)c; (void)device; (void)offset; (void)flags;
    struct obj *o = H(memory);
    if (o && o->kind == K_MEM) {
        *cpu_ptr = o->mem;
        return 0;
    }
    if (o && o->cls == 0xc761) { /* usermode */
        um_map = calloc(1, length);
        *cpu_ptr = um_map;
        if (!db_started) {
            db_started = 1;
            pthread_create(&db_thread, NULL, doorbell_thread, NULL);
        }
        return 0;
    }
    return 0x1f;
}

int crm_unmap_memory(crm_client *c, uint32_t device, uint32_t memory, void *cpu_ptr, uint64_t length, uint32_t flags)
{
    (void)c; (void)device; (void)memory; (void)length; (void)flags;
    if (cpu_ptr == um_map && um_map) {
        atomic_store(&quit, 1);
        if (db_started) {
            pthread_join(db_thread, NULL);
            db_started = 0;
        }
        free(um_map);
        um_map = NULL;
    }
    return 0;
}

int crm_alloc_pages(crm_client *c, uint64_t size, void **addr)
{
    (void)c;
    void *p = NULL;
    if (posix_memalign(&p, 65536, size))
        return -ENOMEM;
    memset(p, 0, size);
    *addr = p;
    return 0;
}
int crm_free_pages(crm_client *c, void *addr, uint64_t size) { (void)c; (void)size; free(addr); return 0; }

int crm_alloc_os_descriptor(crm_client *c, uint32_t device, uint32_t *object, void *addr, uint64_t size, uint32_t flags)
{
    (void)c; (void)device; (void)flags;
    uint32_t h = next_handle++;
    struct obj *o = H(h);
    memset(o, 0, sizeof *o);
    o->kind = K_OSDESC;
    o->mem = addr;
    o->size = size;
    live_objs++;
    *object = h;
    return 0;
}

int crm_map_dma2(crm_client *c, uint32_t device, uint32_t dma, uint32_t memory, uint64_t offset, uint64_t length,
                 uint32_t flags, uint32_t flags2, uint32_t kind, uint64_t *gpu_va)
{
    (void)c; (void)device; (void)dma; (void)offset; (void)flags; (void)flags2; (void)kind;
    struct obj *o = H(memory);
    if (!o || !o->mem)
        return 0x1f;
    unsigned i;
    for (i = 0; i < MAXMAP && maps[i].used; i++)
        ;
    if (i == MAXMAP)
        return -ENOMEM;
    uint64_t va = *gpu_va;
    if (!va) {
        va = next_auto_va;
        next_auto_va += (length + 0x3fffff) & ~0x1fffffull;
    }
    for (unsigned j = 0; j < MAXMAP; j++)
        if (maps[j].used && va < maps[j].va + maps[j].size && va + length > maps[j].va)
            return 0x1f;
    maps[i] = (struct vamap){ va, length, o->mem, 1 };
    *gpu_va = va;
    return 0;
}

int crm_unmap_dma(crm_client *c, uint32_t device, uint32_t dma, uint32_t memory, uint32_t flags, uint64_t gpu_va)
{
    (void)c; (void)device; (void)dma; (void)memory; (void)flags;
    for (unsigned i = 0; i < MAXMAP; i++)
        if (maps[i].used && maps[i].va == gpu_va)
            maps[i].used = 0;
    return 0;
}

void crm_close(crm_client *c) { (void)c; }
