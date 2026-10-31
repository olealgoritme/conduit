/* GPU-visible memory: what the UMD writes for the GPU without a syscall.
 *
 * Method (see README "GPU command capture"):
 *   1. A GPU VA -> CPU VA map built from the D3DKMT calls that create the two
 *      sides: GPU VA ranges from MapGpuVirtualAddress, UpdateGpuVirtualAddress
 *      (MAP/UNMAP), CreateAllocation2/OpenResource2 GpuVirtualAddress,
 *      FreeGpuVirtualAddress; CPU mappings from Lock/Lock2 (pData) and
 *      CreateAllocation pSystemMem; DestroyAllocation and Unlock drop them.
 *   2. At every submission syscall (SubmitCommand, SubmitCommandToHwQueue)
 *      the command buffer named by GPU VA + length is copied through the map
 *      (GPUMEM SUBMIT). If those bytes look like GPFIFO entries (8 bytes,
 *      Volta+ format, most entries naming mapped memory) every entry's
 *      pushbuffer segment is copied too (GPUMEM SEGMENT).
 *   3. A sampler (aux thread every NVTRACE_SNAP_MS, and at fence/present/
 *      doorbell sync points) hashes the pages of watched regions and copies
 *      the pages that changed (GPUMEM SAMPLE). Watched: the allocation that
 *      held a submitted command buffer, WDDM 3.1 doorbell ring buffers,
 *      NVTRACE_WATCH GPU VAs, and with NVTRACE_SNAP=2 every mapped allocation.
 *      This catches user-mode submission (UMD writes GPFIFO + USERD, rings a
 *      doorbell) that never enters the kernel. No guard pages, no debug
 *      registers, no exception handler: nothing that can change how the
 *      traced process runs. Limit: writes that are made and consumed between
 *      two samples are missed; uncached (PAGE_NOCACHE) mappings such as
 *      doorbell and USERD registers are never read.
 *   4. REGION records for every CPU address a call hands out (Lock pData,
 *      doorbell/status/fence CPU VAs, mapped or uncached memory a pointer in
 *      escape/IOCTL output points to), with VirtualQuery's view of it.
 */
#include "nvtrace.h"
#include <string.h>
#include <stdlib.h>

#include "layouts_gen.h"

#define P ((uint32_t)sizeof(void *))
#define PAGE 4096u

static uint64_t rdw(const uint8_t *p, unsigned off, unsigned w)
{
    uint64_t v = 0;
    memcpy(&v, p + off, w);
    return v;
}
#define RD32(b, o) ((uint32_t)rdw((b), (o), 4))
#define RD64(b, o) rdw((b), (o), 8)
#define RDP(b, o) ((uintptr_t)rdw((b), (o), P))

static SRWLOCK gm_lock = SRWLOCK_INIT;
static int gm_ready;

/* ------------------------------------------------------------------ allocations */

typedef struct aent {
    uint32_t h;
    uint32_t used;          /* 0 empty, 1 used, 2 deleted */
    uintptr_t cpu;          /* 0: no CPU mapping known */
    uint64_t cpu_size;
} aent;

#define A_SLOTS 65536u
#define A_PROBES 64
static aent *atab;

static uint32_t ahash(uint32_t h)
{
    h ^= h >> 16;
    h *= 0x7feb352dU;
    h ^= h >> 15;
    return h & (A_SLOTS - 1);
}

static aent *a_find(uint32_t h)
{
    if (!atab || !h)
        return NULL;
    for (uint32_t i = 0, s = ahash(h); i < A_PROBES; i++, s = (s + 1) & (A_SLOTS - 1)) {
        if (!atab[s].used)
            return NULL;
        if (atab[s].used == 1 && atab[s].h == h)
            return &atab[s];
    }
    return NULL;
}

/* lossy: a full probe window reuses its last slot */
static aent *a_get(uint32_t h)
{
    aent *e = a_find(h);
    if (e || !atab || !h)
        return e;
    uint32_t s = ahash(h), last = s;
    for (uint32_t i = 0; i < A_PROBES; i++, s = (s + 1) & (A_SLOTS - 1)) {
        last = s;
        if (atab[s].used != 1)
            break;
    }
    e = &atab[last];
    memset(e, 0, sizeof(*e));
    e->h = h;
    e->used = 1;
    return e;
}

/* ------------------------------------------------------------------ GPU VA ranges */

typedef struct vent {
    uint64_t base, size;    /* size 0: unknown (allocation's CPU size limits it) */
    uint64_t off;           /* byte offset into the allocation */
    uint32_t h;
    uint32_t pad;
} vent;

#define V_MAX 65536u
static vent *vtab;
static uint32_t nv;

static uint32_t v_lower(uint64_t va)   /* first index with base > va */
{
    uint32_t lo = 0, hi = nv;
    while (lo < hi) {
        uint32_t mid = (lo + hi) / 2;
        if (vtab[mid].base <= va)
            lo = mid + 1;
        else
            hi = mid;
    }
    return lo;
}

static void v_remove(uint64_t base, uint64_t size)
{
    if (!vtab || !size)
        return;
    uint64_t end = base + size;
    uint32_t w = 0;
    for (uint32_t i = 0; i < nv; i++) {
        uint64_t e = vtab[i].base + (vtab[i].size ? vtab[i].size : 1);
        if (vtab[i].base < end && e > base)
            continue;
        vtab[w++] = vtab[i];
    }
    nv = w;
}

static void v_remove_alloc(uint32_t h)
{
    uint32_t w = 0;
    for (uint32_t i = 0; vtab && i < nv; i++)
        if (vtab[i].h != h)
            vtab[w++] = vtab[i];
    nv = w;
}

static void v_add(uint64_t base, uint64_t size, uint32_t h, uint64_t off)
{
    if (!vtab || !base || !h)
        return;
    if (size)
        v_remove(base, size);
    else
        for (uint32_t i = 0; i < nv; i++)
            if (vtab[i].base == base) {
                vtab[i].h = h;
                vtab[i].off = off;
                return;
            }
    if (nv == V_MAX)
        return;
    uint32_t pos = v_lower(base);
    memmove(&vtab[pos + 1], &vtab[pos], (nv - pos) * sizeof(vent));
    vtab[pos].base = base;
    vtab[pos].size = size;
    vtab[pos].off = off;
    vtab[pos].h = h;
    nv++;
}

/* GPU VA -> CPU address; *avail = bytes readable there, *h = allocation */
static uintptr_t v_translate(uint64_t va, uint64_t *avail, uint32_t *h)
{
    if (!vtab || !nv)
        return 0;
    uint32_t i = v_lower(va);
    /* the nearest ranges at or below va (a few, for size-0 entries that a
     * later mapping overlaps) */
    for (int k = 0; k < 8 && i > 0; k++) {
        vent *v = &vtab[--i];
        aent *a = a_find(v->h);
        uint64_t rel = va - v->base;
        uint64_t lim = v->size ? v->size : (a ? a->cpu_size : 0);
        if (rel < lim && a && a->cpu) {
            uint64_t aoff = v->off + rel;
            if (aoff < a->cpu_size) {
                uint64_t n = a->cpu_size - aoff;
                if (v->size && lim - rel < n)
                    n = lim - rel;
                *avail = n;
                *h = v->h;
                return a->cpu + (uintptr_t)aoff;
            }
        }
    }
    return 0;
}

static uint64_t region_extent(uintptr_t addr, MEMORY_BASIC_INFORMATION *out)
{
    MEMORY_BASIC_INFORMATION mbi;
    if (!VirtualQuery((void *)addr, &mbi, sizeof(mbi))) {
        memset(out, 0, sizeof(*out));
        return 0;
    }
    *out = mbi;
    if (mbi.State != MEM_COMMIT)
        return 0;
    uint64_t size = (uintptr_t)mbi.BaseAddress + mbi.RegionSize - addr;
    uintptr_t next = (uintptr_t)mbi.BaseAddress + mbi.RegionSize;
    for (int i = 0; i < 64; i++) {
        MEMORY_BASIC_INFORMATION m2;
        if (!VirtualQuery((void *)next, &m2, sizeof(m2)) || m2.AllocationBase != mbi.AllocationBase ||
            m2.State != MEM_COMMIT)
            break;
        size += m2.RegionSize;
        next += m2.RegionSize;
    }
    return size;
}

/* ------------------------------------------------------------------ regions */

void gm_region(uintptr_t addr, uint32_t source, uint32_t call_seq, uint32_t handle, uint32_t offset)
{
    if (!addr || !nvt_cfg.regions)
        return;
    MEMORY_BASIC_INFORMATION mbi;
    ntr_region r;
    memset(&r, 0, sizeof(r));
    r.addr = addr;
    uint64_t sz = region_extent(addr, &mbi);
    r.base = (uintptr_t)mbi.BaseAddress;
    r.size = sz;
    r.alloc_base = (uintptr_t)mbi.AllocationBase;
    r.protect = mbi.State == MEM_COMMIT ? mbi.Protect : 0;
    r.type = mbi.State == MEM_COMMIT ? mbi.Type : 0;
    r.source = source;
    r.call_seq = call_seq;
    r.handle = handle;
    r.offset = offset;
    nvt_buf b = {0};
    nvt_rec_begin(&b, NTR_REC_REGION);
    nvt_buf_put(&b, &r, sizeof(r));
    nvt_rec_end(&b);
    nvt_buf_free(&b);
}

/* Pointer-like values in a returned buffer that point at mapped or uncached
 * memory: candidates for doorbell, USERD and ring mappings handed out by an
 * escape or IOCTL. Each allocation base is reported once. */
#define SEEN_BASES 512
static uintptr_t seen_bases[SEEN_BASES];
static uint32_t nseen_bases;

void gm_scan_returned(const uint8_t *before, const uint8_t *after, size_t len, uint32_t source,
                      uint32_t call_seq)
{
    (void)before;
    if (!nvt_cfg.regions || !after)
        return;
    int found = 0;
    for (size_t o = 0; o + P <= len && found < 16; o += P) {
        uint64_t v = rdw(after, (unsigned)o, P);
        if (!nvt_user_ptr_like(v))
            continue;
        MEMORY_BASIC_INFORMATION mbi;
        if (!VirtualQuery((void *)(uintptr_t)v, &mbi, sizeof(mbi)) || mbi.State != MEM_COMMIT)
            continue;
        int special = mbi.Type == MEM_MAPPED ||
                      (mbi.Protect & (PAGE_NOCACHE | PAGE_WRITECOMBINE)) != 0;
        if (!special)
            continue;
        uintptr_t ab = (uintptr_t)mbi.AllocationBase;
        int dup = 0;
        AcquireSRWLockExclusive(&gm_lock);
        for (uint32_t i = 0; i < nseen_bases && i < SEEN_BASES; i++)
            if (seen_bases[i] == ab) {
                dup = 1;
                break;
            }
        if (!dup)
            seen_bases[nseen_bases++ % SEEN_BASES] = ab;
        ReleaseSRWLockExclusive(&gm_lock);
        if (dup)
            continue;
        gm_region((uintptr_t)v, source, call_seq, 0, (uint32_t)o);
        found++;
    }
}

/* ------------------------------------------------------------------ watched regions */

typedef struct watch {
    uintptr_t cpu;
    uint64_t gpu;           /* GPU VA of cpu (0 if unknown) */
    uint32_t npages;
    uint32_t next;
    uint32_t h;
    uint32_t fails;
    uint32_t live;
    uint32_t id;
    uint32_t ring;          /* doorbell ring / ring control allocation */
    uint64_t *hash;         /* per page; 0 = never sampled */
} watch;

#define MAX_WATCH 128
static watch watches[MAX_WATCH];
static uint32_t nwatch, watch_cursor, watch_ids;
static SRWLOCK snap_lock = SRWLOCK_INIT;   /* one sampler at a time; guards watches[] */
static volatile LONG watch_full_noted;

/* allocations whose CPU mapping should be watched once known (doorbell rings) */
#define MAX_WANT 64
static uint32_t want_h[MAX_WANT];
static uint32_t nwant;

/* NVTRACE_WATCH GPU VA ranges not yet translatable */
typedef struct wspec {
    uint64_t va, size;
    int done;
} wspec;
static wspec wspecs[16];
static int nwspecs;

int gm_has_watch(void)
{
    return nwatch != 0;
}

static void emit_watch(const watch *w);

/* called with snap_lock held */
static void watch_add_locked(uintptr_t cpu, uint64_t size, uint64_t gpu, uint32_t h, int ring)
{
    if (!cpu || !size || !nvt_cfg.snap)
        return;
    MEMORY_BASIC_INFORMATION mbi;
    if (!VirtualQuery((void *)cpu, &mbi, sizeof(mbi)) || mbi.State != MEM_COMMIT ||
        (mbi.Protect & (PAGE_NOCACHE | PAGE_GUARD | PAGE_NOACCESS)))
        return;   /* never read device registers or guard pages */
    uintptr_t start = cpu & ~(uintptr_t)(PAGE - 1);
    if (gpu)
        gpu -= cpu - start;
    if (size > nvt_cfg.snap_region)
        size = nvt_cfg.snap_region;
    uint64_t npages = (cpu - start + size + PAGE - 1) / PAGE;
    for (uint32_t i = 0; i < nwatch; i++) {
        watch *w = &watches[i];
        if (w->live && start >= w->cpu && start < w->cpu + (uint64_t)w->npages * PAGE) {
            w->ring |= ring;
            return;
        }
    }
    if (nwatch == MAX_WATCH) {
        if (!InterlockedExchange(&watch_full_noted, 1))
            nvt_note(1, "sampler: %u regions watched, further regions ignored", MAX_WATCH);
        return;
    }
    uint64_t *hash = VirtualAlloc(NULL, npages * 8, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE);
    if (!hash)
        return;
    watch *w = &watches[nwatch];
    memset(w, 0, sizeof(*w));
    w->cpu = start;
    w->gpu = gpu;
    w->npages = (uint32_t)npages;
    w->h = h;
    w->hash = hash;
    w->live = 1;
    w->ring = ring;
    w->id = ++watch_ids;
    nwatch++;
    emit_watch(w);
}

static void watch_add(uintptr_t cpu, uint64_t size, uint64_t gpu, uint32_t h, int ring)
{
    AcquireSRWLockExclusive(&snap_lock);
    watch_add_locked(cpu, size, gpu, h, ring);
    ReleaseSRWLockExclusive(&snap_lock);
}

static void watch_drop_alloc(uint32_t h)
{
    if (!h || !nwatch)
        return;
    AcquireSRWLockExclusive(&snap_lock);
    for (uint32_t i = 0; i < nwatch; i++)
        if (watches[i].live && watches[i].h == h)
            watches[i].live = 0;
    ReleaseSRWLockExclusive(&snap_lock);
}

static void emit_gpumem_hdr(nvt_buf *b, const ntr_gpumem *g)
{
    nvt_rec_begin(b, NTR_REC_GPUMEM);
    nvt_buf_put(b, g, sizeof(*g));
}

static void emit_watch(const watch *w)
{
    nvt_buf b = {0};
    ntr_gpumem g;
    memset(&g, 0, sizeof(g));
    g.kind = NTR_GPUMEM_WATCH;
    g.gpu_va = w->gpu;
    g.cpu_va = w->cpu;
    g.req_size = w->npages * PAGE;
    g.hallocation = w->h;
    g.index = w->id;
    emit_gpumem_hdr(&b, &g);
    nvt_rec_end(&b);
    nvt_buf_free(&b);
}

static uint64_t fnv64(const uint8_t *p, size_t n)
{
    uint64_t h = 0xcbf29ce484222325ull;
    const uint64_t *q = (const uint64_t *)p;
    for (size_t i = 0; i < n / 8; i++) {
        h ^= q[i];
        h *= 0x100000001b3ull;
    }
    return h | 1;   /* never 0: 0 means "not sampled yet" */
}

/* One sampler pass within the time budget. sync: called from a submission
 * or fence call on an application thread (skipped if the sampler is busy). */
static volatile LONG64 last_sync_qpc;
static uint8_t snap_page[PAGE] __attribute__((aligned(16)));
static nvt_buf snap_buf;

/* Read page i of w; emit it when its hash changed. snap_lock held.
 * Returns 0 if the page could not be read. */
static int sample_page(watch *w, uint32_t i, uint32_t flags)
{
    uintptr_t a = w->cpu + (uintptr_t)i * PAGE;
    /* the allocation may have been unmapped and the range reused: never
     * touch guard, no-access or uncached pages (reading a guard page would
     * consume it and break the owner's stack growth) */
    MEMORY_BASIC_INFORMATION mbi;
    if (!VirtualQuery((void *)a, &mbi, sizeof(mbi)) || mbi.State != MEM_COMMIT ||
        (mbi.Protect & (PAGE_GUARD | PAGE_NOACCESS | PAGE_NOCACHE)) || !(mbi.Protect & 0xEE)) {
        if (++w->fails > 64)
            w->live = 0;
        return 0;
    }
    size_t got = nvt_safe_copy(snap_page, (const void *)a, PAGE);
    if (got != PAGE) {
        if (++w->fails > 64)
            w->live = 0;   /* unmapped */
        return 0;
    }
    uint64_t hv = fnv64(snap_page, PAGE);
    if (hv != w->hash[i]) {
        ntr_gpumem g;
        memset(&g, 0, sizeof(g));
        g.kind = NTR_GPUMEM_SAMPLE;
        g.flags = (w->hash[i] ? 0 : NTR_GM_FIRST) | flags;
        g.gpu_va = w->gpu ? w->gpu + (uint64_t)i * PAGE : 0;
        g.cpu_va = a;
        g.req_size = PAGE;
        g.size = PAGE;
        g.hallocation = w->h;
        g.index = i;
        g.aux = hv;
        g.queue = w->id;
        emit_gpumem_hdr(&snap_buf, &g);
        nvt_buf_put(&snap_buf, snap_page, PAGE);
        nvt_rec_end(&snap_buf);
        w->hash[i] = hv;
    }
    return 1;
}

static void snap_done(void)
{
    if (snap_buf.cap > (1u << 20))
        nvt_buf_free(&snap_buf);
}

/* One sampler pass within the time budget. sync: called from a submission
 * or fence call on an application thread (skipped if the sampler is busy). */
void gm_sample_tick(int sync)
{
    if (!nvt_cfg.snap || !nwatch)
        return;
    if (sync) {
        /* at most one sync-point pass per millisecond, small budget: the
         * application thread waits for it */
        LARGE_INTEGER now;
        QueryPerformanceCounter(&now);
        if (now.QuadPart - last_sync_qpc < nvt_qpc_freq.QuadPart / 1000)
            return;
        if (!TryAcquireSRWLockExclusive(&snap_lock))
            return;
        last_sync_qpc = now.QuadPart;
    } else {
        AcquireSRWLockExclusive(&snap_lock);
    }
    LARGE_INTEGER q0, q;
    QueryPerformanceCounter(&q0);
    uint32_t us = sync ? (nvt_cfg.snap_us < 50 ? nvt_cfg.snap_us : 50) : nvt_cfg.snap_us;
    uint64_t budget = (uint64_t)nvt_qpc_freq.QuadPart * (us ? us : 1) / 1000000u;
    int stop = 0;
    for (uint32_t k = 0; k < nwatch && !stop; k++) {
        watch *w = &watches[(watch_cursor + k) % nwatch];
        if (!w->live)
            continue;
        for (uint32_t n = 0; n < w->npages; n++) {
            uint32_t i = w->next;
            w->next = (i + 1) % w->npages;
            if (!sample_page(w, i, sync ? NTR_GM_SYNC : 0))
                break;
            QueryPerformanceCounter(&q);
            if ((uint64_t)(q.QuadPart - q0.QuadPart) > budget) {
                stop = 1;
                break;
            }
        }
    }
    watch_cursor = nwatch ? (watch_cursor + 1) % nwatch : 0;
    snap_done();
    ReleaseSRWLockExclusive(&snap_lock);
}

/* The pages of the watched allocation that holds cpu (the memory around a
 * submitted command buffer), from the submitted page onwards and wrapping,
 * within NVTRACE_SNAP_SUBMIT_US; the timer sampler covers the rest. Only
 * changed pages are written. */
void gm_snap_full(uintptr_t cpu)
{
    if (!nvt_cfg.snap || !cpu || !nwatch)
        return;
    AcquireSRWLockExclusive(&snap_lock);
    LARGE_INTEGER q0, q;
    QueryPerformanceCounter(&q0);
    uint64_t budget = (uint64_t)nvt_qpc_freq.QuadPart * (nvt_cfg.snap_submit_us ? nvt_cfg.snap_submit_us : 1) /
                      1000000u;
    for (uint32_t k = 0; k < nwatch; k++) {
        watch *w = &watches[k];
        if (!w->live || cpu < w->cpu || cpu >= w->cpu + (uint64_t)w->npages * PAGE)
            continue;
        uint32_t first = (uint32_t)((cpu - w->cpu) / PAGE);
        for (uint32_t n = 0; n < w->npages; n++) {
            if (!sample_page(w, (first + n) % w->npages, NTR_GM_SYNC))
                break;
            QueryPerformanceCounter(&q);
            if ((uint64_t)(q.QuadPart - q0.QuadPart) > budget)
                break;
        }
        break;
    }
    snap_done();
    ReleaseSRWLockExclusive(&snap_lock);
}

/* Every page of the doorbell ring buffers (at NotifyWorkSubmission). */
void gm_snap_rings(void)
{
    if (!nvt_cfg.snap || !nwatch)
        return;
    AcquireSRWLockExclusive(&snap_lock);
    LARGE_INTEGER q0, q;
    QueryPerformanceCounter(&q0);
    uint64_t budget = (uint64_t)nvt_qpc_freq.QuadPart * (nvt_cfg.snap_submit_us ? nvt_cfg.snap_submit_us : 1) /
                      1000000u;
    int stop = 0;
    for (uint32_t k = 0; k < nwatch && !stop; k++) {
        watch *w = &watches[k];
        if (!w->live || !w->ring)
            continue;
        for (uint32_t n = 0; n < w->npages; n++) {
            uint32_t i = w->next;
            w->next = (i + 1) % w->npages;
            if (!sample_page(w, i, NTR_GM_SYNC))
                break;
            QueryPerformanceCounter(&q);
            if ((uint64_t)(q.QuadPart - q0.QuadPart) > budget) {
                stop = 1;
                break;
            }
        }
    }
    snap_done();
    ReleaseSRWLockExclusive(&snap_lock);
}

/* ------------------------------------------------------------------ submissions */

/* Append one GPUMEM record (call_seq filled in later) with up to `n` bytes
 * read from cpu. Returns the bytes stored. */
static uint32_t stash(nvt_buf *gb, ntr_gpumem *g, uintptr_t cpu, uint64_t n)
{
    size_t start = gb->len;
    ntr_rec_hdr h;
    LARGE_INTEGER q;
    QueryPerformanceCounter(&q);
    memset(&h, 0, sizeof(h));
    h.type = NTR_REC_GPUMEM;
    h.qpc = (uint64_t)q.QuadPart;
    h.tid = GetCurrentThreadId();
    if (!nvt_buf_put(gb, &h, sizeof(h)))
        return 0;
    size_t goff = gb->len;
    if (!nvt_buf_put(gb, g, sizeof(*g)))
        return 0;
    size_t got = 0;
    if (cpu && n && !nvt_buf_reserve(gb, (size_t)n + 8)) {
        got = nvt_safe_copy(gb->p + gb->len, (const void *)cpu, (size_t)n);
        gb->len += got;
    }
    ntr_gpumem *gg = (ntr_gpumem *)(gb->p + goff);
    gg->size = (uint32_t)got;
    if (got < n)
        gg->flags |= NTR_GM_FAULT;
    nvt_buf_pad8(gb);
    if (gb->oom)
        return 0;
    ((ntr_rec_hdr *)(gb->p + start))->size = (uint32_t)(gb->len - start);
    return (uint32_t)got;
}

/* Copy the command buffer [va, va+len) and, if it is a GPFIFO, its segments. */
static void capture_submit(nvt_buf *gb, uint64_t va, uint32_t len, uint32_t queue, uint64_t fence)
{
    uint64_t budget = nvt_cfg.pushbuf;
    ntr_gpumem g;
    memset(&g, 0, sizeof(g));
    g.kind = NTR_GPUMEM_SUBMIT;
    g.gpu_va = va;
    g.req_size = len;
    g.queue = queue;
    g.fence = fence;
    uint64_t avail = 0;
    uint32_t h = 0;
    AcquireSRWLockShared(&gm_lock);
    uintptr_t cpu = v_translate(va, &avail, &h);
    aent *ae = h ? a_find(h) : NULL;
    uintptr_t acpu = ae ? ae->cpu : 0;
    uint64_t asize = ae ? ae->cpu_size : 0;
    ReleaseSRWLockShared(&gm_lock);
    uint64_t n = len;
    if (!cpu) {
        g.flags |= NTR_GM_UNMAPPED;
        n = 0;
    } else {
        if (n > avail)
            n = avail;
        if (n > budget) {
            n = budget;
            g.flags |= NTR_GM_TRUNCATED;
        }
    }
    g.cpu_va = cpu;
    g.hallocation = h;
    size_t data_off = gb->len + sizeof(ntr_rec_hdr) + sizeof(ntr_gpumem);
    uint32_t got = stash(gb, &g, cpu, n);
    budget -= got;
    if (cpu && nvt_cfg.snap)
        watch_add(acpu ? acpu : cpu, acpu ? asize : n, acpu ? va - (cpu - acpu) : va, h, 0);
    if (!nvt_cfg.gpfifo || !got || (got & 7) || got > 65536)
        return;

    /* GPFIFO entries? (NVC96F/NVCA6F_GP_ENTRY: GET 31:2 + GET_HI 7:0,
     * LENGTH 30:10 in dwords; length 0 = control entry, opcode 4 sets
     * PB_EXTENDED_BASE bits 24:8 of entry0 = VA bits 56:40) */
    uint32_t ne = got / 8, valid = 0, data_entries = 0;
    uint64_t ext = 0;
    for (uint32_t i = 0; i < ne; i++) {
        uint32_t e0 = RD32(gb->p + data_off, i * 8), e1 = RD32(gb->p + data_off, i * 8 + 4);
        uint32_t dw = (e1 >> 10) & 0x1FFFFF;
        if (!dw) {
            if ((e1 & 0xFF) == 4)
                ext = (uint64_t)((e0 >> 8) & 0x1FFFF) << 40;
            continue;
        }
        data_entries++;
        uint64_t sva = ext | ((uint64_t)(e1 & 0xFF) << 32) | (e0 & ~3u);
        uint64_t sav;
        uint32_t sh;
        AcquireSRWLockShared(&gm_lock);
        uintptr_t sc = v_translate(sva, &sav, &sh);
        ReleaseSRWLockShared(&gm_lock);
        if (sc && sav >= (uint64_t)dw * 4)
            valid++;
    }
    if (!data_entries || valid * 2 < data_entries)
        return;
    ext = 0;
    for (uint32_t i = 0; i < ne && budget; i++) {
        uint32_t e0 = RD32(gb->p + data_off, i * 8), e1 = RD32(gb->p + data_off, i * 8 + 4);
        uint32_t dw = (e1 >> 10) & 0x1FFFFF;
        if (!dw) {
            if ((e1 & 0xFF) == 4)
                ext = (uint64_t)((e0 >> 8) & 0x1FFFF) << 40;
            continue;
        }
        uint64_t sva = ext | ((uint64_t)(e1 & 0xFF) << 32) | (e0 & ~3u);
        uint64_t sav = 0;
        uint32_t sh = 0;
        AcquireSRWLockShared(&gm_lock);
        uintptr_t sc = v_translate(sva, &sav, &sh);
        ReleaseSRWLockShared(&gm_lock);
        ntr_gpumem s;
        memset(&s, 0, sizeof(s));
        s.kind = NTR_GPUMEM_SEGMENT;
        s.gpu_va = sva;
        s.cpu_va = sc;
        s.req_size = dw * 4;
        s.hallocation = sh;
        s.queue = queue;
        s.fence = fence;
        s.index = i;
        s.aux = ((uint64_t)e1 << 32) | e0;
        uint64_t sn = (uint64_t)dw * 4;
        if (!sc) {
            s.flags |= NTR_GM_UNMAPPED;
            sn = 0;
        } else {
            if (sn > sav)
                sn = sav;
            if (sn > budget) {
                sn = budget;
                s.flags |= NTR_GM_TRUNCATED;
            }
        }
        /* data_off stays valid: stash may move gb->p, so re-read it each time */
        budget -= stash(gb, &s, sc, sn);
    }
}

void gm_before(nvt_hook *h, uintptr_t *args, nvt_buf *gb)
{
    uint8_t s[0x60];
    if (!gm_ready || !nvt_cfg.pushbuf)
        return;
    if (h->gk == GK_SUBMIT) {
        if (!nvt_safe_read(s, args[0], NVT_OFF_SUBMITCOMMAND_BroadcastContext + 4))
            return;
        capture_submit(gb, RD64(s, NVT_OFF_SUBMITCOMMAND_Commands),
                       RD32(s, NVT_OFF_SUBMITCOMMAND_CommandLength),
                       RD32(s, NVT_OFF_SUBMITCOMMAND_BroadcastContext), 0);
    } else if (h->gk == GK_SUBMITHWQ) {
        if (!nvt_safe_read(s, args[0], NVT_OFF_SUBMITCOMMANDTOHWQUEUE_CommandLength + 4))
            return;
        capture_submit(gb, RD64(s, NVT_OFF_SUBMITCOMMANDTOHWQUEUE_CommandBuffer),
                       RD32(s, NVT_OFF_SUBMITCOMMANDTOHWQUEUE_CommandLength),
                       RD32(s, NVT_OFF_SUBMITCOMMANDTOHWQUEUE_hHwQueue),
                       RD64(s, NVT_OFF_SUBMITCOMMANDTOHWQUEUE_HwQueueProgressFenceId));
    }
}

void gm_flush_pending(nvt_buf *gb, uint32_t call_seq)
{
    size_t off = 0;
    while (off + sizeof(ntr_rec_hdr) <= gb->len) {
        ntr_rec_hdr *rh = (ntr_rec_hdr *)(gb->p + off);
        if (rh->size < sizeof(ntr_rec_hdr) || off + rh->size > gb->len)
            break;
        ntr_gpumem *g = (ntr_gpumem *)(rh + 1);
        g->call_seq = call_seq;
        rh->seq = nvt_next_seq();
        nvt_log_submit(rh, rh->size);
        off += rh->size;
    }
    gb->len = 0;
    gb->oom = 0;
    if (gb->cap > (8u << 20))
        nvt_buf_free(gb);
}

/* ------------------------------------------------------------------ after calls */

static void try_watch_specs(void)
{
    for (int i = 0; i < nwspecs; i++) {
        if (wspecs[i].done)
            continue;
        uint64_t avail = 0;
        uint32_t h = 0;
        AcquireSRWLockShared(&gm_lock);
        uintptr_t cpu = v_translate(wspecs[i].va, &avail, &h);
        ReleaseSRWLockShared(&gm_lock);
        if (!cpu)
            continue;
        uint64_t n = wspecs[i].size && wspecs[i].size < avail ? wspecs[i].size : avail;
        wspecs[i].done = 1;
        nvt_note(0, "sampler: NVTRACE_WATCH 0x%llx -> cpu %p (%llu bytes, alloc 0x%x)",
                 (unsigned long long)wspecs[i].va, (void *)cpu, (unsigned long long)n, h);
        watch_add(cpu, n, wspecs[i].va, h, 0);
    }
}

static void cpu_mapped(uint32_t h, uintptr_t cpu, uint32_t source, uint32_t call_seq)
{
    MEMORY_BASIC_INFORMATION mbi;
    uint64_t sz = region_extent(cpu, &mbi);
    int want = 0;
    AcquireSRWLockExclusive(&gm_lock);
    aent *a = a_get(h);
    if (a) {
        a->cpu = cpu;
        a->cpu_size = sz;
    }
    for (uint32_t i = 0; i < nwant; i++)
        if (want_h[i] == h)
            want = 1;
    ReleaseSRWLockExclusive(&gm_lock);
    gm_region(cpu, source, call_seq, h, 0);
    if (want || (nvt_cfg.snap >= 2 && sz && sz <= nvt_cfg.snap_region))
        watch_add(cpu, sz, 0, h, want);
    if (nwspecs)
        try_watch_specs();
}

static void alloc_gone(uint32_t h)
{
    AcquireSRWLockExclusive(&gm_lock);
    aent *a = a_find(h);
    if (a)
        a->used = 2;
    v_remove_alloc(h);
    ReleaseSRWLockExclusive(&gm_lock);
    watch_drop_alloc(h);
}

void gm_after(nvt_hook *h, uintptr_t *args, int ok, uint32_t seq)
{
    uint8_t s[0x100];
    if (!gm_ready || !ok)
        return;
    switch (h->gk) {
    case GK_LOCK:
        if (nvt_safe_read(s, args[0], NVT_OFF_LOCK_pData + P))
            cpu_mapped(RD32(s, NVT_OFF_LOCK_hAllocation), RDP(s, NVT_OFF_LOCK_pData), NTR_RGN_LOCK, seq);
        break;
    case GK_LOCK2:
        if (nvt_safe_read(s, args[0], NVT_OFF_LOCK2_pData + P))
            cpu_mapped(RD32(s, NVT_OFF_LOCK2_hAllocation), RDP(s, NVT_OFF_LOCK2_pData), NTR_RGN_LOCK2,
                       seq);
        break;
    case GK_UNLOCK2:
    case GK_UNLOCK: {
        uint32_t list[256], n = 1;
        if (h->gk == GK_UNLOCK2) {
            if (!nvt_safe_read(s, args[0], NVT_OFF_UNLOCK2_hAllocation + 4))
                break;
            list[0] = RD32(s, NVT_OFF_UNLOCK2_hAllocation);
        } else {
            if (!nvt_safe_read(s, args[0], NVT_OFF_UNLOCK_phAllocations + P))
                break;
            n = RD32(s, NVT_OFF_UNLOCK_NumAllocations);
            if (n > 256)
                n = 256;
            if (!nvt_safe_read(list, RDP(s, NVT_OFF_UNLOCK_phAllocations), n * 4))
                break;
        }
        for (uint32_t i = 0; i < n; i++) {
            AcquireSRWLockExclusive(&gm_lock);
            aent *a = a_find(list[i]);
            if (a)
                a->cpu = 0;
            ReleaseSRWLockExclusive(&gm_lock);
            watch_drop_alloc(list[i]);
        }
        break;
    }
    case GK_CREATEALLOC:
    case GK_CREATEALLOC2: {
        if (!nvt_safe_read(s, args[0], NVT_OFF_CREATEALLOCATION_pAllocationInfo + P))
            break;
        uint32_t n = RD32(s, NVT_OFF_CREATEALLOCATION_NumAllocations);
        uintptr_t info = RDP(s, NVT_OFF_CREATEALLOCATION_pAllocationInfo);
        uint32_t es = h->gk == GK_CREATEALLOC2 ? NVT_SIZE_ALLOCATIONINFO2 : NVT_SIZE_ALLOCATIONINFO;
        for (uint32_t i = 0; i < n && i < 4096; i++) {
            uint8_t e[NVT_SIZE_ALLOCATIONINFO2 > NVT_SIZE_ALLOCATIONINFO ? NVT_SIZE_ALLOCATIONINFO2
                                                                        : NVT_SIZE_ALLOCATIONINFO];
            if (!nvt_safe_read(e, info + (uintptr_t)i * es, es))
                break;
            uint32_t ha = RD32(e, h->gk == GK_CREATEALLOC2 ? NVT_OFF_ALLOCATIONINFO2_hAllocation
                                                          : NVT_OFF_ALLOCATIONINFO_hAllocation);
            uintptr_t sys = RDP(e, h->gk == GK_CREATEALLOC2 ? NVT_OFF_ALLOCATIONINFO2_pSystemMem
                                                           : NVT_OFF_ALLOCATIONINFO_pSystemMem);
            /* pSystemMem shares a union with hSection in ALLOCATIONINFO2;
             * only a pointer into committed memory counts */
            MEMORY_BASIC_INFORMATION mbi;
            if (sys && nvt_user_ptr_like(sys) && VirtualQuery((void *)sys, &mbi, sizeof(mbi)) &&
                mbi.State == MEM_COMMIT)
                cpu_mapped(ha, sys, NTR_RGN_SYSMEM, seq);
            if (h->gk == GK_CREATEALLOC2) {
                uint64_t va = RD64(e, NVT_OFF_ALLOCATIONINFO2_GpuVirtualAddress);
                if (va) {
                    AcquireSRWLockExclusive(&gm_lock);
                    v_add(va, 0, ha, 0);
                    ReleaseSRWLockExclusive(&gm_lock);
                }
            }
        }
        break;
    }
    case GK_DESTROYALLOC: {
        if (!nvt_safe_read(s, args[0], NVT_OFF_DESTROYALLOCATION_AllocationCount + 4))
            break;
        uint32_t n = RD32(s, NVT_OFF_DESTROYALLOCATION_AllocationCount);
        uintptr_t list = RDP(s, NVT_OFF_DESTROYALLOCATION_phAllocationList);
        for (uint32_t i = 0; i < n && i < 65536; i++) {
            uint32_t ha;
            if (!nvt_safe_read(&ha, list + (uintptr_t)i * 4, 4))
                break;
            alloc_gone(ha);
        }
        break;
    }
    case GK_MAPGPUVA:
        if (nvt_safe_read(s, args[0], NVT_OFF_MAPGPUVIRTUALADDRESS_VirtualAddress + 8)) {
            AcquireSRWLockExclusive(&gm_lock);
            v_add(RD64(s, NVT_OFF_MAPGPUVIRTUALADDRESS_VirtualAddress),
                  RD64(s, NVT_OFF_MAPGPUVIRTUALADDRESS_SizeInPages) * PAGE,
                  RD32(s, NVT_OFF_MAPGPUVIRTUALADDRESS_hAllocation),
                  RD64(s, NVT_OFF_MAPGPUVIRTUALADDRESS_OffsetInPages) * PAGE);
            ReleaseSRWLockExclusive(&gm_lock);
            if (nwspecs)
                try_watch_specs();
        }
        break;
    case GK_UPDATEGPUVA: {
        if (!nvt_safe_read(s, args[0], NVT_OFF_UPDATEGPUVIRTUALADDRESS_Operations + P))
            break;
        uint32_t n = RD32(s, NVT_OFF_UPDATEGPUVIRTUALADDRESS_NumOperations);
        uintptr_t ops = RDP(s, NVT_OFF_UPDATEGPUVIRTUALADDRESS_Operations);
        for (uint32_t i = 0; i < n && i < 65536; i++) {
            uint8_t o[NVT_SIZE_UPDATEGPUVA_OPERATION];
            if (!nvt_safe_read(o, ops + (uintptr_t)i * NVT_SIZE_UPDATEGPUVA_OPERATION, sizeof(o)))
                break;
            uint32_t type = RD32(o, NVT_OFF_UPDATEGPUVA_OPERATION_OperationType);
            uint64_t base = RD64(o, NVT_OFF_UPDATEGPUVA_OPERATION_BaseAddress);
            uint64_t size = RD64(o, NVT_OFF_UPDATEGPUVA_OPERATION_Size);
            AcquireSRWLockExclusive(&gm_lock);
            if (type == 0 || type == 3)   /* MAP, MAP_PROTECT */
                v_add(base, size, RD32(o, NVT_OFF_UPDATEGPUVA_OPERATION_hAllocation),
                      RD64(o, NVT_OFF_UPDATEGPUVA_OPERATION_AllocationOffset));
            else if (type == 1)          /* UNMAP */
                v_remove(base, size);
            ReleaseSRWLockExclusive(&gm_lock);
        }
        if (nwspecs)
            try_watch_specs();
        break;
    }
    case GK_FREEGPUVA:
        if (nvt_safe_read(s, args[0], NVT_OFF_FREEGPUVIRTUALADDRESS_Size + 8)) {
            AcquireSRWLockExclusive(&gm_lock);
            v_remove(RD64(s, NVT_OFF_FREEGPUVIRTUALADDRESS_BaseAddress),
                     RD64(s, NVT_OFF_FREEGPUVIRTUALADDRESS_Size));
            ReleaseSRWLockExclusive(&gm_lock);
        }
        break;
    case GK_OPENRESOURCE:
    case GK_OPENRESOURCENT: {
        unsigned on = h->gk == GK_OPENRESOURCE ? NVT_OFF_OPENRESOURCE_NumAllocations
                                               : NVT_OFF_OPENRESOURCEFROMNTHANDLE_NumAllocations;
        unsigned oi = h->gk == GK_OPENRESOURCE ? NVT_OFF_OPENRESOURCE_pOpenAllocationInfo
                                               : NVT_OFF_OPENRESOURCEFROMNTHANDLE_pOpenAllocationInfo2;
        if (!nvt_safe_read(s, args[0], oi + P))
            break;
        uint32_t n = RD32(s, on);
        uintptr_t info = RDP(s, oi);
        for (uint32_t i = 0; i < n && i < 4096; i++) {
            uint8_t e[NVT_SIZE_OPENALLOCATIONINFO2];
            if (!nvt_safe_read(e, info + (uintptr_t)i * sizeof(e), sizeof(e)))
                break;
            uint64_t va = RD64(e, NVT_OFF_OPENALLOCATIONINFO2_GpuVirtualAddress);
            if (va) {
                AcquireSRWLockExclusive(&gm_lock);
                v_add(va, 0, RD32(e, NVT_OFF_OPENALLOCATIONINFO2_hAllocation), 0);
                ReleaseSRWLockExclusive(&gm_lock);
            }
        }
        break;
    }
    case GK_CREATEDOORBELL:
        if (nvt_safe_read(s, args[0], NVT_OFF_CREATE_DOORBELL_hDoorbell + 4)) {
            uint32_t hd = RD32(s, NVT_OFF_CREATE_DOORBELL_hDoorbell);
            gm_region(RDP(s, NVT_OFF_CREATE_DOORBELL_DoorbellCPUVirtualAddress), NTR_RGN_DOORBELL, seq, hd, 0);
            gm_region(RDP(s, NVT_OFF_CREATE_DOORBELL_DoorbellSecondaryCPUVirtualAddress), NTR_RGN_DOORBELL2,
                      seq, hd, 0);
            gm_region(RDP(s, NVT_OFF_CREATE_DOORBELL_DoorbellStatusCPUVirtualAddress),
                      NTR_RGN_DOORBELL_STATUS, seq, hd, 0);
            gm_region(RDP(s, NVT_OFF_CREATE_DOORBELL_HwQueueProgressFenceLastQueuedValueCPUVirtualAddress),
                      NTR_RGN_QUEUED_FENCE, seq, hd, 0);
            uint32_t rings[2] = {RD32(s, NVT_OFF_CREATE_DOORBELL_hRingBuffer),
                                 RD32(s, NVT_OFF_CREATE_DOORBELL_hRingBufferControl)};
            for (int i = 0; i < 2; i++) {
                if (!rings[i])
                    continue;
                AcquireSRWLockExclusive(&gm_lock);
                aent *a = a_find(rings[i]);
                uintptr_t cpu = a ? a->cpu : 0;
                uint64_t sz = a ? a->cpu_size : 0;
                if (nwant < MAX_WANT)
                    want_h[nwant++] = rings[i];
                ReleaseSRWLockExclusive(&gm_lock);
                if (cpu)
                    watch_add(cpu, sz, 0, rings[i], 1);
            }
        }
        break;
    case GK_CREATEHWQUEUE:
        if (nvt_safe_read(s, args[0], NVT_OFF_CREATEHWQUEUE_HwQueueProgressFenceCPUVirtualAddress + P))
            gm_region(RDP(s, NVT_OFF_CREATEHWQUEUE_HwQueueProgressFenceCPUVirtualAddress),
                      NTR_RGN_HWQUEUE_FENCE, seq, 0, 0);
        break;
    case GK_CREATEPAGINGQUEUE:
        if (nvt_safe_read(s, args[0], NVT_OFF_CREATEPAGINGQUEUE_FenceValueCPUVirtualAddress + P))
            gm_region(RDP(s, NVT_OFF_CREATEPAGINGQUEUE_FenceValueCPUVirtualAddress), NTR_RGN_PAGING_FENCE,
                      seq, 0, 0);
        break;
    case GK_CREATESYNC2:
        if (nvt_safe_read(s, args[0], NVT_OFF_CREATESYNCHRONIZATIONOBJECT2_FenceValueCPUVirtualAddress + P))
            gm_region(RDP(s, NVT_OFF_CREATESYNCHRONIZATIONOBJECT2_FenceValueCPUVirtualAddress), NTR_RGN_FENCE,
                      seq, 0, 0);
        break;
    case GK_CREATENATIVEFENCE:
        if (nvt_safe_read(s, args[0], NVT_OFF_CREATENATIVEFENCE_CurrentValueCpuVa + P))
            gm_region(RDP(s, NVT_OFF_CREATENATIVEFENCE_CurrentValueCpuVa), NTR_RGN_FENCE, seq, 0, 0);
        break;
    case GK_OPENNATIVEFENCE:
        if (nvt_safe_read(s, args[0], NVT_OFF_OPENNATIVEFENCEFROMNTHANDLE_CurrentValueCpuVa + P))
            gm_region(RDP(s, NVT_OFF_OPENNATIVEFENCEFROMNTHANDLE_CurrentValueCpuVa), NTR_RGN_FENCE, seq, 0,
                      0);
        break;
    case GK_OPENSYNC2:
        if (nvt_safe_read(s, args[0], NVT_OFF_OPENSYNCOBJECTFROMNTHANDLE2_FenceValueCPUVirtualAddress + P))
            gm_region(RDP(s, NVT_OFF_OPENSYNCOBJECTFROMNTHANDLE2_FenceValueCPUVirtualAddress), NTR_RGN_FENCE,
                      seq, 0, 0);
        break;
    case GK_SUBMIT:
    case GK_SUBMITHWQ: {
        uint64_t va = 0;
        if (h->gk == GK_SUBMIT && nvt_safe_read(s, args[0], NVT_OFF_SUBMITCOMMAND_Commands + 8))
            va = RD64(s, NVT_OFF_SUBMITCOMMAND_Commands);
        else if (h->gk == GK_SUBMITHWQ &&
                 nvt_safe_read(s, args[0], NVT_OFF_SUBMITCOMMANDTOHWQUEUE_CommandBuffer + 8))
            va = RD64(s, NVT_OFF_SUBMITCOMMANDTOHWQUEUE_CommandBuffer);
        uint64_t avail = 0;
        uint32_t ha = 0;
        AcquireSRWLockShared(&gm_lock);
        uintptr_t cpu = va ? v_translate(va, &avail, &ha) : 0;
        ReleaseSRWLockShared(&gm_lock);
        if (cpu && nvt_cfg.snap_submit)
            gm_snap_full(cpu);
        else
            gm_sample_tick(1);
        break;
    }
    case GK_NOTIFYWORK:
        gm_snap_rings();
        gm_sample_tick(1);
        break;
    case GK_SYNCPOINT:
        gm_sample_tick(1);
        break;
    default:
        break;
    }
}

/* ------------------------------------------------------------------ init */

void gm_init(void)
{
    atab = VirtualAlloc(NULL, A_SLOTS * sizeof(aent), MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE);
    vtab = VirtualAlloc(NULL, (V_MAX + 1) * sizeof(vent), MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE);
    /* NVTRACE_WATCH=0xVA[:size],... */
    const char *w = nvt_cfg.watch;
    while (*w && nwspecs < 16) {
        char *end;
        uint64_t va = _strtoui64(w, &end, 0);
        uint64_t size = 0;
        if (end == w)
            break;
        if (*end == ':')
            size = _strtoui64(end + 1, &end, 0);
        wspecs[nwspecs].va = va;
        wspecs[nwspecs].size = size;
        nwspecs++;
        while (*end == ',' || *end == ' ')
            end++;
        w = end;
    }
    gm_ready = atab && vtab;
}
