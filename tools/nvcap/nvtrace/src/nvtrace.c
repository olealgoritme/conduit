/* nvtrace: capture every D3DKMT thunk (win32u NtGdiDdDDI* and NtDxgk*),
 * device I/O on NVIDIA/graphics device handles, section maps, module loads
 * and the GPU command buffers of the process it is loaded into. See
 * README.md for the log format. */
#include "nvtrace.h"
#include <string.h>
#include <stdlib.h>
#include <stdio.h>
#include <stdarg.h>
#include <wchar.h>

#include "layouts_gen.h"

#define NFDESCS (sizeof(nvt_fdescs) / sizeof(nvt_fdescs[0]))
#define P ((uint32_t)sizeof(void *))

/* runtime labels after the generated ones */
enum {
    LBL_RAW = NVT_NLABELS,
    LBL_CHASE,
    LBL_CMDBUF,
    LBL_ALLOCLIST,
    LBL_PATCHLIST,
    LBL_COUNT
};
static const char *const extra_labels[] = {
    "pData(raw)", "chase", "Render.CommandBuffer", "Render.AllocationList",
    "Render.PatchLocationList",
};

nvt_config nvt_cfg;
volatile LONG nvt_dead;
volatile LONG nvt_log_ready;
volatile LONG nvt_exiting;
LARGE_INTEGER nvt_qpc_freq;
DWORD nvt_tls = TLS_OUT_OF_INDEXES;

static HMODULE self_module;
static wchar_t self_path[MAX_PATH];
static wchar_t self_dir[MAX_PATH];
static void *dll_cookie;
static volatile LONG started;       /* full start done (or in progress) */
static volatile LONG start_failed;
static char start_reason[96] = "load";
static FILETIME start_ft;

/* ------------------------------------------------------------------ hooks */

#define MAX_HOOKS 1024
static nvt_hook *hooks;
static volatile LONG nhooks;
static volatile LONG win32u_hooked, ntdll_hooked, kernelbase_hooked;

/* ------------------------------------------------------------------ threads */

#define MAX_DEPTH 4
typedef struct tstate {
    int busy;
    int depth;
    nvt_buf bufs[MAX_DEPTH];
    nvt_buf gbufs[MAX_DEPTH];       /* GPUMEM records stashed before the call */
} tstate;

static HANDLE ts_heap;

static tstate *ts_get(void)
{
    if (nvt_tls == TLS_OUT_OF_INDEXES)
        return NULL;
    void *v = TlsGetValue(nvt_tls);
    if (v == (void *)1)
        return NULL;                    /* nvtrace's own threads */
    if (!v) {
        if (!ts_heap)
            return NULL;
        v = HeapAlloc(ts_heap, HEAP_ZERO_MEMORY, sizeof(tstate));
        if (!v)
            return NULL;
        TlsSetValue(nvt_tls, v);
    }
    return v;
}

static void ts_free(void)
{
    if (nvt_tls == TLS_OUT_OF_INDEXES)
        return;
    tstate *t = TlsGetValue(nvt_tls);
    if (!t || t == (void *)1)
        return;
    /* other DLLs' thread-detach code may still call hooked functions on this
     * thread: mark it pass-through instead of allocating a new state */
    TlsSetValue(nvt_tls, (void *)1);
    for (int i = 0; i < MAX_DEPTH; i++) {
        nvt_buf_free(&t->bufs[i]);
        nvt_buf_free(&t->gbufs[i]);
    }
    HeapFree(ts_heap, 0, t);
}

void nvt_mark_thread_internal(void)
{
    if (nvt_tls != TLS_OUT_OF_INDEXES)
        TlsSetValue(nvt_tls, (void *)1);
}

/* For code that logs outside a hook (selftest, loader callback): marks the
 * thread busy so hooked calls made meanwhile pass through. */
int nvt_thread_enter(void)
{
    tstate *t = ts_get();
    if (!t)
        return TlsGetValue(nvt_tls) == (void *)1;
    if (t->busy)
        return 0;
    t->busy = 1;
    return 1;
}

void nvt_thread_leave(void)
{
    tstate *t = ts_get();
    if (t)
        t->busy = 0;
}

/* ------------------------------------------------------------------ safe reads */

/* Memory named by the traced program is read with NtReadVirtualMemory on our
 * own process: a bad pointer gives a short copy, never an exception that the
 * program's vectored handlers (or a debugger) would see. */
typedef LONG(NTAPI *NtReadVirtualMemory_t)(HANDLE, PVOID, PVOID, SIZE_T, PSIZE_T);
static NtReadVirtualMemory_t p_read_vm;

size_t nvt_safe_copy(void *dst, const void *src, size_t n)
{
    SIZE_T got = 0;
    if (!n || !src || !p_read_vm)
        return 0;
    LONG st = p_read_vm((HANDLE)(intptr_t)-1, (PVOID)src, dst, n, &got);
    if (st >= 0)
        return n;
    return got <= n ? got : 0;
}

int nvt_safe_read(void *dst, uintptr_t src, size_t n)
{
    if (!src)
        return 0;
    return nvt_safe_copy(dst, (const void *)src, n) == n;
}
#define safe_read nvt_safe_read

int nvt_user_ptr_like(uint64_t v)
{
#ifdef _WIN64
    return v >= 0x10000 && v < 0x00007FFFFFFF0000ull && (v & 3) == 0;
#else
    return v >= 0x10000 && v < 0xFFFF0000ull && (v & 3) == 0;
#endif
}
#define user_ptr_like nvt_user_ptr_like

/* lower-case ASCII copy of an NT object name */
static void name_from_oa(uintptr_t oa, char *out, size_t outsz, int *relative)
{
    uintptr_t root = 0, us = 0, buf = 0;
    uint16_t len = 0;
    wchar_t w[260];
    out[0] = 0;
    *relative = 0;
    if (!safe_read(&root, oa + NVT_OFF_OBJECT_ATTRIBUTES_RootDirectory, P) ||
        !safe_read(&us, oa + NVT_OFF_OBJECT_ATTRIBUTES_ObjectName, P) || !us)
        return;
    if (!safe_read(&len, us + NVT_OFF_UNICODE_STRING_Length, 2) ||
        !safe_read(&buf, us + NVT_OFF_UNICODE_STRING_Buffer, P) || !buf)
        return;
    *relative = root != 0;
    size_t nch = len / 2;
    if (nch > 259)
        nch = 259;
    size_t got = nvt_safe_copy(w, (const void *)buf, nch * 2) / 2;
    size_t i;
    for (i = 0; i < got && i + 1 < outsz; i++) {
        wchar_t c = w[i];
        if (c >= 'A' && c <= 'Z')
            c = c - 'A' + 'a';
        out[i] = (c < 128 && c >= 32) ? (char)c : '?';
    }
    out[i] = 0;
}

/* ------------------------------------------------------------------ NVIDIA modules */

/* address ranges of loaded nv*.dll (not nvtrace): "was this call made from
 * an NVIDIA DLL" for file opens */
typedef struct nvmod {
    uintptr_t base, end;
} nvmod;
#define MAX_NVMOD 64
static nvmod nvmods[MAX_NVMOD];
static SRWLOCK nvmod_lock = SRWLOCK_INIT;

static void nvmod_update(const wchar_t *basename, size_t len, uintptr_t base, uint64_t size, int add)
{
    if (len < 2 || (basename[0] != L'n' && basename[0] != L'N') || (basename[1] != L'v' && basename[1] != L'V'))
        return;
    if (len >= 7 && !_wcsnicmp(basename, L"nvtrace", 7))
        return;
    if (len < 5 || _wcsnicmp(basename + len - 4, L".dll", 4))
        return;   /* NVIDIA's drivers are DLLs (not nvinject.exe and the like) */
    AcquireSRWLockExclusive(&nvmod_lock);
    int slot = -1;
    for (int i = 0; i < MAX_NVMOD; i++) {
        if (nvmods[i].base == base) {
            slot = i;
            break;
        }
        if (slot < 0 && !nvmods[i].base)
            slot = i;
    }
    if (slot >= 0) {
        nvmods[slot].base = add ? base : 0;
        nvmods[slot].end = add ? base + (uintptr_t)size : 0;
    }
    ReleaseSRWLockExclusive(&nvmod_lock);
}

int nv_module_at(uintptr_t a)
{
    int hit = 0;
    AcquireSRWLockShared(&nvmod_lock);
    for (int i = 0; i < MAX_NVMOD; i++)
        if (nvmods[i].base && a >= nvmods[i].base && a < nvmods[i].end) {
            hit = 1;
            break;
        }
    ReleaseSRWLockShared(&nvmod_lock);
    return hit;
}

/* NVIDIA-related file paths (shader caches, DriverStore, NVIDIA folders) */
static int file_interesting(const char *n)
{
    static const char *const pat[] = {"nvidia", "driverstore", "\\nv", "dxcache", "glcache", "nv_cache",
                                      "d3dscache", "shadercache", "nvcache", "computecache", "nvapi",
                                      "\\device\\nv", NULL};
    for (int i = 0; pat[i]; i++)
        if (strstr(n, pat[i]))
            return 1;
    return 0;
}

/* ------------------------------------------------------------------ Render */

typedef struct ctxbuf {
    uint32_t h;
    uintptr_t cmd, alloc, patch;
    uint32_t cmdsize, allocsize, patchsize;
} ctxbuf;

#define MAX_CTX 128
static ctxbuf ctx_tab[MAX_CTX];
static SRWLOCK ctx_lock = SRWLOCK_INIT;

static void ctx_set(uint32_t h, uintptr_t cmd, uint32_t cmdsize, uintptr_t alloc,
                    uint32_t allocsize, uintptr_t patch, uint32_t patchsize)
{
    if (!h)
        return;
    AcquireSRWLockExclusive(&ctx_lock);
    int slot = -1;
    for (int i = 0; i < MAX_CTX; i++) {
        if (ctx_tab[i].h == h) {
            slot = i;
            break;
        }
        if (slot < 0 && !ctx_tab[i].h)
            slot = i;
    }
    if (slot >= 0) {
        ctxbuf *c = &ctx_tab[slot];
        c->h = h;
        c->cmd = cmd;
        c->cmdsize = cmdsize;
        c->alloc = alloc;
        c->allocsize = allocsize;
        c->patch = patch;
        c->patchsize = patchsize;
    }
    ReleaseSRWLockExclusive(&ctx_lock);
}

static int ctx_get(uint32_t h, ctxbuf *out)
{
    int ok = 0;
    AcquireSRWLockShared(&ctx_lock);
    for (int i = 0; i < MAX_CTX; i++)
        if (ctx_tab[i].h == h) {
            *out = ctx_tab[i];
            ok = 1;
            break;
        }
    ReleaseSRWLockShared(&ctx_lock);
    return ok;
}

/* ------------------------------------------------------------------ capture */

struct capture {
    nvt_buf *b;
    uint32_t nblobs;            /* blobs in the record so far */
    uint32_t flags;             /* NTR_CALL_* */
    size_t first_blob;          /* offset of the first blob */
    uint32_t before_blobs;      /* blobs written in phase 0 */
    uint64_t chase_budget;      /* bytes left for chased copies */
    uint64_t rec_budget;        /* bytes left for this record (NVTRACE_MAXREC) */
    size_t hint_off;            /* find_before: where the last match was */
    uint32_t hint_i;
};

static uint64_t rd_le(const uint8_t *p, int w)
{
    uint64_t v = 0;
    memcpy(&v, p, w);
    return v;
}

/* The after-walk visits blobs in the before-walk's order, so the search
 * starts after the previous match: linear overall, not quadratic. */
static size_t find_before(capture *c, uint32_t label, uint32_t index, uint64_t addr,
                          uint32_t *cap_out)
{
    for (int pass = 0; pass < 2; pass++) {
        size_t off = pass ? c->first_blob : c->hint_off;
        uint32_t i = pass ? 0 : c->hint_i;
        uint32_t stop = pass ? c->hint_i : c->before_blobs;
        if (!pass && !off) {
            off = c->first_blob;
            i = 0;
        }
        for (; i < stop && off < c->b->len; i++) {
            ntr_blob *bl = (ntr_blob *)(c->b->p + off);
            size_t next = off + sizeof(ntr_blob) + ((bl->cap_size + 7) & ~7u);
            if (bl->label == label && bl->index == index && bl->addr == addr && bl->phase == 0 &&
                !(bl->flags & NTR_BLOB_SAME)) {
                *cap_out = bl->cap_size;
                c->hint_off = next;
                c->hint_i = i + 1;
                return off + sizeof(ntr_blob);
            }
            off = next;
        }
    }
    return 0;
}

/* Append one blob. Returns the offset of its data in c->b (0 on failure) and
 * the stored size in *stored. */
static size_t add_blob(capture *c, uint32_t label, uint32_t index, uintptr_t addr,
                       uint64_t req, int phase, uint16_t parent, uint8_t extra_flags,
                       uint32_t *stored)
{
    uint64_t cap = req;
    uint8_t flags = extra_flags;
    *stored = 0;
    if (cap > nvt_cfg.maxbuf) {
        cap = nvt_cfg.maxbuf;
        flags |= NTR_BLOB_TRUNCATED;
        c->flags |= NTR_CALL_TRUNCATED;
    }
    if (cap > c->rec_budget) {          /* whole-record safety cap */
        cap = c->rec_budget;
        flags |= NTR_BLOB_TRUNCATED;
        c->flags |= NTR_CALL_TRUNCATED;
    }
    if (cap > 0xFFFFFFF0u)
        cap = 0xFFFFFFF0u;
    if (nvt_buf_reserve(c->b, sizeof(ntr_blob) + cap + 8))
        return 0;
    size_t hoff = c->b->len;
    ntr_blob *bl = (ntr_blob *)(c->b->p + hoff);
    memset(bl, 0, sizeof(*bl));
    bl->label = label;
    bl->index = index;
    bl->addr = addr;
    bl->req_size = req > 0xFFFFFFFFu ? 0xFFFFFFFFu : (uint32_t)req;
    bl->phase = (uint8_t)phase;
    bl->parent = parent;
    c->b->len += sizeof(ntr_blob);
    size_t doff = c->b->len;
    size_t got = cap ? nvt_safe_copy(c->b->p + doff, (const void *)addr, (size_t)cap) : 0;
    if (got < cap) {
        flags |= NTR_BLOB_FAULT;
        c->flags |= NTR_CALL_FAULT;
    }
    if (phase == 1 && got) {
        uint32_t bcap = 0;
        size_t boff = find_before(c, label, index, addr, &bcap);
        if (boff && bcap == got && !memcmp(c->b->p + boff, c->b->p + doff, got)) {
            flags |= NTR_BLOB_SAME;
            /* keep the data offset valid for children: point at the before copy */
            bl = (ntr_blob *)(c->b->p + hoff);
            bl->cap_size = 0;
            bl->flags = flags;
            c->nblobs++;
            *stored = (uint32_t)got;
            return boff;
        }
    }
    bl = (ntr_blob *)(c->b->p + hoff);
    bl->cap_size = (uint32_t)got;
    bl->flags = flags;
    c->rec_budget -= got < c->rec_budget ? got : c->rec_budget;
    c->b->len += got;
    nvt_buf_pad8(c->b);
    c->nblobs++;
    *stored = (uint32_t)got;
    return doff;
}

static int readable_region(uintptr_t a, size_t *avail)
{
    MEMORY_BASIC_INFORMATION mbi;
    if (!VirtualQuery((void *)a, &mbi, sizeof(mbi)))
        return 0;
    if (mbi.State != MEM_COMMIT)
        return 0;
    DWORD pr = mbi.Protect;
    if (pr & (PAGE_GUARD | PAGE_NOACCESS | PAGE_NOCACHE | PAGE_WRITECOMBINE))
        return 0;
    if (!(pr & (PAGE_READONLY | PAGE_READWRITE | PAGE_WRITECOPY | PAGE_EXECUTE_READ |
                PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY)))
        return 0;
    *avail = (uintptr_t)mbi.BaseAddress + mbi.RegionSize - a;
    return 1;
}

static void chase(capture *c, size_t doff, uint32_t len, uint16_t parent, int phase,
                  uint32_t depth)
{
    uint64_t seen[64];
    uint32_t nseen = 0, done = 0;
    if (!nvt_cfg.chase || depth >= nvt_cfg.chase_depth)
        return;
    for (uint32_t o = 0; o + P <= len && done < nvt_cfg.chase_max; o += P) {
        uint64_t v = rd_le(c->b->p + doff + o, P);
        if (!user_ptr_like(v))
            continue;
        int dup = 0;
        for (uint32_t i = 0; i < nseen; i++)
            if (seen[i] == v)
                dup = 1;
        if (dup || nseen == 64)
            continue;
        seen[nseen++] = v;
        size_t avail;
        if (!readable_region((uintptr_t)v, &avail))
            continue;
        uint64_t n = nvt_cfg.chase;
        if (n > avail)
            n = avail;
        if (n > c->chase_budget)
            break;
        c->chase_budget -= n;
        uint32_t stored;
        uint16_t me = (uint16_t)(c->nblobs + 1);
        size_t cd = add_blob(c, LBL_CHASE, o, (uintptr_t)v, n, phase, parent, NTR_BLOB_CHASED,
                             &stored);
        done++;
        /* add_blob may move c->b->p: cd is an offset, re-read through it */
        if (cd && stored)
            chase(c, cd, stored, me, phase, depth + 1);
    }
}

static void walk(capture *c, const nvt_pdesc *pd, size_t poff, uint32_t plen, uint16_t parent,
                 uint32_t index, int phase, int depth)
{
    if (depth > 6 || !(pd->flags & (phase ? NVT_PF_OUT : NVT_PF_IN)))
        return;
    if ((uint32_t)pd->off + P > plen)
        return;
    uintptr_t ptr = (uintptr_t)rd_le(c->b->p + poff + pd->off, P);
    if (ptr < 0x10000 || !user_ptr_like(ptr & ~(uintptr_t)3))
        return;                         /* NULL, small handle or value in a union */
    uint64_t req = 0;
    switch (pd->kind) {
    case NVT_SZ_FIXED:
        req = pd->fixed;
        break;
    case NVT_SZ_FIELD:
    case NVT_SZ_FIELD1:
        if ((uint32_t)pd->szoff + pd->szw > plen)
            return;
        req = (rd_le(c->b->p + poff + pd->szoff, pd->szw) + (pd->kind == NVT_SZ_FIELD1)) *
              (uint64_t)pd->fixed;
        break;
    case NVT_SZ_WSTR: {
        wchar_t chunk[256];
        uint64_t n = 0;
        int done = 0;
        while (!done && n < 32768) {
            size_t got = nvt_safe_copy(chunk, (const void *)(ptr + (uintptr_t)n * 2), sizeof(chunk)) / 2;
            if (!got)
                break;
            for (size_t i = 0; i < got; i++) {
                n++;
                if (!chunk[i]) {
                    done = 1;
                    break;
                }
            }
            if (got < 256)
                break;
        }
        req = n * 2;
        break;
    }
    }
    uint32_t stored;
    uint16_t me = (uint16_t)(c->nblobs + 1);
    size_t doff = add_blob(c, pd->label, index, ptr, req, phase, parent, 0, &stored);
    if (!doff || !stored)
        return;
    if (pd->nchild) {
        uint32_t elem = pd->elem ? pd->elem : stored;
        uint32_t n = stored / elem;
        if (n > 65536)
            n = 65536;
        for (uint32_t e = 0; e < n; e++)
            for (uint32_t k = 0; k < pd->nchild; k++)
                walk(c, &nvt_pdescs[pd->child + k], doff + (size_t)e * elem, elem, me,
                     pd->elem ? e : 0xFFFFFFFFu, phase, depth + 1);
    }
    if (pd->flags & NVT_PF_CHASE)
        chase(c, doff, stored, me, phase, 0);
}

static void render_extra(capture *c, uintptr_t *args)
{
    uint8_t r[0x180];
    ctxbuf cb;
    if (!safe_read(r, args[0], NVT_OFF_RENDER_NewPatchLocationListSize + 4))
        return;
    uint32_t h = (uint32_t)rd_le(r + NVT_OFF_RENDER_hContext, 4);
    if (!ctx_get(h, &cb))
        return;
    uint32_t off = (uint32_t)rd_le(r + NVT_OFF_RENDER_CommandOffset, 4);
    uint32_t len = (uint32_t)rd_le(r + NVT_OFF_RENDER_CommandLength, 4);
    uint32_t na = (uint32_t)rd_le(r + NVT_OFF_RENDER_AllocationCount, 4);
    uint32_t np = (uint32_t)rd_le(r + NVT_OFF_RENDER_PatchLocationCount, 4);
    uint32_t stored;
    if (cb.cmd && len) {
        uint64_t n = len;
        if (nvt_cfg.cmdbuf && n > nvt_cfg.cmdbuf)
            n = nvt_cfg.cmdbuf;
        add_blob(c, LBL_CMDBUF, off, cb.cmd + off, n, 0, 1, 0, &stored);
    }
    if (cb.alloc && na)
        add_blob(c, LBL_ALLOCLIST, 0xFFFFFFFFu, cb.alloc, (uint64_t)na * 8, 0, 1, 0, &stored);
    if (cb.patch && np)
        add_blob(c, LBL_PATCHLIST, 0xFFFFFFFFu, cb.patch, (uint64_t)np * 24, 0, 1, 0, &stored);
}

static void render_after(uintptr_t *args)
{
    uint8_t r[0x180];
    if (!safe_read(r, args[0], NVT_OFF_RENDER_NewPatchLocationListSize + 4))
        return;
    ctx_set((uint32_t)rd_le(r + NVT_OFF_RENDER_hContext, 4),
            (uintptr_t)rd_le(r + NVT_OFF_RENDER_pNewCommandBuffer, P),
            (uint32_t)rd_le(r + NVT_OFF_RENDER_NewCommandBufferSize, 4),
            (uintptr_t)rd_le(r + NVT_OFF_RENDER_pNewAllocationList, P),
            (uint32_t)rd_le(r + NVT_OFF_RENDER_NewAllocationListSize, 4),
            (uintptr_t)rd_le(r + NVT_OFF_RENDER_pNewPatchLocationList, P),
            (uint32_t)rd_le(r + NVT_OFF_RENDER_NewPatchLocationListSize, 4));
}

static void ctx_after(const nvt_hook *h, uintptr_t *args)
{
    uint8_t r[0x100];
    if (strstr(h->name, "CreateContext")) {
        if (!safe_read(r, args[0], NVT_OFF_CREATECONTEXT_PatchLocationListSize + 4))
            return;
        ctx_set((uint32_t)rd_le(r + NVT_OFF_CREATECONTEXT_hContext, 4),
                (uintptr_t)rd_le(r + NVT_OFF_CREATECONTEXT_pCommandBuffer, P),
                (uint32_t)rd_le(r + NVT_OFF_CREATECONTEXT_CommandBufferSize, 4),
                (uintptr_t)rd_le(r + NVT_OFF_CREATECONTEXT_pAllocationList, P),
                (uint32_t)rd_le(r + NVT_OFF_CREATECONTEXT_AllocationListSize, 4),
                (uintptr_t)rd_le(r + NVT_OFF_CREATECONTEXT_pPatchLocationList, P),
                (uint32_t)rd_le(r + NVT_OFF_CREATECONTEXT_PatchLocationListSize, 4));
    } else {
        if (!safe_read(r, args[0], NVT_OFF_CREATEDEVICE_PatchLocationListSize + 4))
            return;
        ctx_set((uint32_t)rd_le(r + NVT_OFF_CREATEDEVICE_hDevice, 4),
                (uintptr_t)rd_le(r + NVT_OFF_CREATEDEVICE_pCommandBuffer, P),
                (uint32_t)rd_le(r + NVT_OFF_CREATEDEVICE_CommandBufferSize, 4),
                (uintptr_t)rd_le(r + NVT_OFF_CREATEDEVICE_pAllocationList, P),
                (uint32_t)rd_le(r + NVT_OFF_CREATEDEVICE_AllocationListSize, 4),
                (uintptr_t)rd_le(r + NVT_OFF_CREATEDEVICE_pPatchLocationList, P),
                (uint32_t)rd_le(r + NVT_OFF_CREATEDEVICE_PatchLocationListSize, 4));
    }
}

static void capture_phase(capture *c, nvt_hook *h, size_t args_off, int phase,
                          uintptr_t *live_args)
{
    if (phase == 0)
        c->first_blob = c->b->len;
    if (h->fd) {
        for (uint32_t i = 0; i < h->fd->pcount; i++)
            walk(c, &nvt_pdescs[h->fd->pfirst + i], args_off, h->nargs * P, 0, 0xFFFFFFFFu,
                 phase, 0);
    } else if (h->hookflags & NTR_HOOK_RAW) {
        uint32_t stored;
        if (user_ptr_like(live_args[0]))
            add_blob(c, LBL_RAW, 0xFFFFFFFFu, live_args[0], nvt_cfg.rawsize, phase, 0, 0, &stored);
    }
    if (phase == 0 && (h->flags & NVT_FN_RENDER))
        render_extra(c, live_args);
    if (phase == 0)
        c->before_blobs = c->nblobs;
}

/* Returned pointers to mapped/uncached memory in escape private data and
 * IOCTL output (doorbell, USERD, ring mappings handed to the UMD). */
static void scan_outputs(nvt_hook *h, uintptr_t *args, uint32_t seq)
{
    static const uint32_t max = 65536;
    uintptr_t p = 0;
    uint32_t n = 0, src = 0;
    if (h->gk == GK_ESCAPE) {
        uint8_t e[NVT_OFF_ESCAPE_PrivateDriverDataSize + 4];
        if (!safe_read(e, args[0], sizeof(e)))
            return;
        p = (uintptr_t)rd_le(e + NVT_OFF_ESCAPE_pPrivateDriverData, P);
        n = (uint32_t)rd_le(e + NVT_OFF_ESCAPE_PrivateDriverDataSize, 4);
        src = NTR_RGN_ESCAPE;
    } else {
        p = args[8];
        n = (uint32_t)args[9];
        src = NTR_RGN_IOCTL;
    }
    if (!p || !n)
        return;
    if (n > max)
        n = max;
    nvt_buf b = {0};
    if (nvt_buf_reserve(&b, n))
        return;
    size_t got = nvt_safe_copy(b.p, (const void *)p, n);
    gm_scan_returned(NULL, b.p, got, src, seq);
    nvt_buf_free(&b);
}

/* ------------------------------------------------------------------ frames */

typedef USHORT(WINAPI *RtlCaptureStackBackTrace_t)(ULONG, ULONG, PVOID *, PULONG);
static RtlCaptureStackBackTrace_t p_capture_stack;

static uint32_t get_frames(void *ret, uint64_t *out, uint32_t max)
{
    void *fr[64];
    uint32_t n = 0;
    out[0] = (uintptr_t)ret;
    if (max <= 1 || !p_capture_stack)
        return 1;
    uint32_t want = max + 4 > 64 ? 64 : max + 4;
    USHORT got = p_capture_stack(0, want, fr, NULL);
    uint32_t start = 0xFFFFFFFFu;
    for (uint32_t i = 0; i < got; i++)
        if (fr[i] == ret) {
            start = i;
            break;
        }
    if (start == 0xFFFFFFFFu)
        return 1;
    for (uint32_t i = start; i < got && n < max; i++)
        out[n++] = (uintptr_t)fr[i];
    return n ? n : 1;
}

/* ------------------------------------------------------------------ dispatch */

static void finish(int at_exit);

static uintptr_t passthrough(nvt_hook *h, uintptr_t *args, DWORD le)
{
    SetLastError(le);
    return nvt_call_orig(h->orig, args, h->nargs);
}

#ifdef _WIN64
uintptr_t nvt_dispatch(nvt_hook *h, uintptr_t *args, void *ret);
uintptr_t nvt_dispatch(nvt_hook *h, uintptr_t *args, void *ret)
#else
static uintptr_t dispatch_impl(nvt_hook *h, uintptr_t *args, void *ret)
#endif
{
    DWORD le = GetLastError();
    if (nvt_dead || !nvt_log_ready || nvt_log_owner == GetCurrentThreadId())
        return passthrough(h, args, le);
    tstate *t = ts_get();
    if (!t || t->busy || t->depth >= MAX_DEPTH)
        return passthrough(h, args, le);
    if (h->handler) {
        SetLastError(le);
        return h->handler(h, args, ret);
    }
    if (h->flags & NVT_FN_TERMINATE) {
        HANDLE ph = (HANDLE)args[0];
        if (!ph) {
            /* ExitProcess is killing the other threads. One of them may hold
             * one of our locks when it dies; hold the heap locks across the
             * kill, and from then on log only D3DKMT calls (the UMDs' DllMain
             * teardown) on paths that take no SRW lock and the log lock only
             * by try-lock. */
            HANDLE th = ts_heap;
            if (th)
                HeapLock(th);
            nvt_heap_lock(1);
            nvt_log_exit_mode();
            SetLastError(le);
            uintptr_t r = nvt_call_orig(h->orig, args, h->nargs);
            DWORD le2 = GetLastError();
            InterlockedExchange(&nvt_exiting, 1);
            nvt_heap_lock(0);
            if (th)
                HeapUnlock(th);
            SetLastError(le2);
            return r;
        }
        /* TerminateProcess on ourselves */
        if (ph == (HANDLE)(intptr_t)-1 || GetProcessId(ph) == GetCurrentProcessId()) {
            t->busy = 1;
            nvt_note(0, "NtTerminateProcess(self, 0x%lx)", (unsigned long)args[1]);
            finish(0);
            t->busy = 0;
        }
        return passthrough(h, args, le);
    }
    int exiting = nvt_exiting;
    if (exiting && (h->flags & ~(NVT_FN_RAW | NVT_FN_CTXTRACK)) != 0)
        return passthrough(h, args, le);   /* only plain D3DKMT calls at exit */
    if (exiting && strcmp(h->module, "win32u.dll"))
        return passthrough(h, args, le);
    int key_close = 0;
    if (h->flags & NVT_FN_CLOSE) {
        HANDLE ch = (HANDLE)args[0];
        key_close = reg_tracked(ch);
        if (!key_close && !dev_tracked(ch)) {
            dev_on_close(ch);
            reg_on_close(ch);
            return passthrough(h, args, le);
        }
    }
    if ((h->flags & NVT_FN_DEVHANDLE) && !dev_tracked((HANDLE)args[0])) {
        int tracked = 0;
        /* name the handle only for IOCTL device types that NVIDIA/graphics
         * drivers use (FILE_DEVICE_UNKNOWN 0x22, VIDEO 0x23, vendor >=
         * 0x8000): never consoles, pipes or sockets, whose synchronous file
         * objects can block a name query */
        uint32_t dtype = (uint32_t)args[5] >> 16;
        if (h->gk == GK_IOCTL && (dtype == 0x22 || dtype == 0x23 || dtype >= 0x8000) &&
            strcmp(h->name, "NtDeviceIoControlFile") == 0) {
            t->busy = 1;
            tracked = dev_probe((HANDLE)args[0]);
            t->busy = 0;
        }
        if (!tracked)
            return passthrough(h, args, le);
    }
    if ((h->flags & NVT_FN_REGKEY) && !reg_tracked((HANDLE)args[0])) {
        if (!nvt_cfg.reg)
            return passthrough(h, args, le);
        t->busy = 1;
        int tracked = reg_probe((HANDLE)args[0]);
        t->busy = 0;
        if (!tracked)
            return passthrough(h, args, le);
    }
    if ((h->flags & NVT_FN_REGOPEN) && !nvt_cfg.reg)
        return passthrough(h, args, le);
    if ((h->flags & NVT_FN_MAP) && !nvt_cfg.maps)
        return passthrough(h, args, le);

    t->busy = 1;
    nvt_buf *b = &t->bufs[t->depth];
    nvt_buf *gb = &t->gbufs[t->depth];
    capture c;
    memset(&c, 0, sizeof(c));
    c.b = b;
    c.chase_budget = (uint64_t)nvt_cfg.chase * nvt_cfg.chase_max * 4;
    c.rec_budget = nvt_cfg.maxrec;

    uint64_t frames[64];
    uint32_t nframes = get_frames(ret, frames, nvt_cfg.stack ? nvt_cfg.stack : 1);
    nvt_rec_begin(b, NTR_REC_CALL);
    size_t call_off = b->len;
    ntr_call *cl = nvt_buf_put(b, NULL, sizeof(ntr_call));
    if (cl) {
        cl->hook_id = h->id;
        cl->nargs = (uint16_t)h->nargs;
        cl->nframes = (uint16_t)nframes;
        cl->depth = (uint32_t)t->depth;
    }
    for (uint32_t i = 0; i < h->nargs; i++) {
        uint64_t v = args[i];
        nvt_buf_put(b, &v, 8);
    }
    nvt_buf_put(b, frames, (size_t)nframes * 8);
    /* native slot copy for the layout walker (not part of the format: it is
     * removed again before the record is written) */
    size_t slots_off = b->len;
    nvt_buf_put(b, args, (size_t)h->nargs * P);
    nvt_buf_pad8(b);
    size_t slots_end = b->len;
    capture_phase(&c, h, slots_off, 0, args);
    gb->len = 0;
    gb->oom = 0;
    if ((h->gk == GK_SUBMIT || h->gk == GK_SUBMITHWQ) && !exiting)
        gm_before(h, args, gb);
    t->depth++;
    t->busy = 0;

    LARGE_INTEGER q0, q1;
    QueryPerformanceCounter(&q0);
    SetLastError(le);
    uintptr_t r = nvt_call_orig(h->orig, args, h->nargs);
    DWORD le2 = GetLastError();
    QueryPerformanceCounter(&q1);

    t->busy = 1;
    t->depth--;
    int keep = 1;
    int ok = (h->flags & NVT_FN_PROCESS) ? r != 0 : (LONG)r >= 0;
    if (h->flags & NVT_FN_OPEN) {
        char name[200];
        int rel;
        name_from_oa(args[2], name, sizeof(name), &rel);
        HANDLE fh = NULL;
        safe_read(&fh, args[0], P);
        if (!rel && dev_name_interesting(name)) {
            if (ok)
                dev_track(fh, name);     /* failed opens are logged too */
        } else if (nvt_cfg.files >= 2) {
            keep = 1;
        } else if (nvt_cfg.files == 1) {
            /* files the NVIDIA DLLs open, and NVIDIA-related paths */
            keep = file_interesting(name);
            for (uint32_t i = 0; i < nframes && !keep; i++)
                keep = nv_module_at((uintptr_t)frames[i]);
        } else {
            keep = 0;
        }
    }
    if (h->flags & NVT_FN_REGOPEN) {
        char name[200];
        int rel;
        name_from_oa(args[2], name, sizeof(name), &rel);
        HANDLE kh = NULL;
        if (ok)
            safe_read(&kh, args[0], P);
        HANDLE root = NULL;
        safe_read(&root, args[2] + NVT_OFF_OBJECT_ATTRIBUTES_RootDirectory, P);
        keep = reg_after_open(kh, name, root, ok);
    }
    if (keep) {
        capture_phase(&c, h, slots_off, 1, args);
        if ((h->flags & NVT_FN_CTXTRACK) && ok && !exiting)
            ctx_after(h, args);
        if ((h->flags & NVT_FN_RENDER) && ok)
            render_after(args);
        if ((h->flags & NVT_FN_CLOSE) && ok) {
            dev_untrack((HANDLE)args[0]);
            dev_on_close((HANDLE)args[0]);
            if (key_close)
                reg_on_close((HANDLE)args[0]);
        }
        if (!b->oom) {
            ntr_rec_hdr *rh = (ntr_rec_hdr *)b->p;
            rh->qpc = (uint64_t)q0.QuadPart;
            cl = (ntr_call *)(b->p + call_off);
            cl->qpc_end = (uint64_t)q1.QuadPart;
#ifdef _WIN64
            cl->retval = (uint64_t)r;
#else
            cl->retval = (uint64_t)(int64_t)(int32_t)r;
#endif
            cl->flags = c.flags | ((h->hookflags & NTR_HOOK_RAW) ? NTR_CALL_RAW : 0);
            memmove(b->p + slots_off, b->p + slots_end, b->len - slots_end);
            b->len -= slots_end - slots_off;
        }
        uint32_t seq = nvt_rec_end(b);
        InterlockedIncrement64(&nvt_stat_calls);
        if (gb->len)
            gm_flush_pending(gb, seq);
        if (h->gk && !exiting) {
            gm_after(h, args, ok, seq);
            if (h->gk == GK_IOCTL && (uint32_t)r == 0x103u)
                dev_pending_add(args, seq, 0);
            else if (h->gk == GK_IOREAD && (uint32_t)r == 0x103u)
                dev_pending_add_buf(args, seq, args[5], (uint32_t)args[6], 0);
            else if (h->gk == GK_IOWRITE && (uint32_t)r == 0x103u)
                dev_pending_add_buf(args, seq, 0, 0, 0);
            else if ((h->gk == GK_ESCAPE || h->gk == GK_IOCTL) && ok)
                scan_outputs(h, args, seq);
        }
    }
    gb->len = 0;
    if (b->cap > (16u << 20))
        nvt_buf_free(b);
    t->busy = 0;
    SetLastError(le2);
    return r;
}

#ifndef _WIN64
uint64_t nvt_dispatch(nvt_hook *h, uintptr_t *args, void *ret);
uint64_t nvt_dispatch(nvt_hook *h, uintptr_t *args, void *ret)
{
    uint32_t ab = h->argbytes;
    uint32_t r = (uint32_t)dispatch_impl(h, args, ret);
    return ((uint64_t)ab << 32) | r;
}
#endif

/* ------------------------------------------------------------------ child processes */

/* LoadLibraryW(nvtrace) as a user APC on the child's initial thread: it runs
 * when that thread finishes loader initialisation, before the entry point,
 * after the parent resumed it. Nothing runs in the child before the parent
 * (a sandbox broker, say) has set it up, and the parent does not wait. */
static int inject_same_arch(HANDLE proc, HANDLE thread)
{
    size_t bytes = (wcslen(self_path) + 1) * sizeof(wchar_t);
    void *rp = VirtualAllocEx(proc, NULL, bytes, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    if (!rp)
        return -1;
    if (!WriteProcessMemory(proc, rp, self_path, bytes, NULL)) {
        VirtualFreeEx(proc, rp, 0, MEM_RELEASE);
        return -1;
    }
    FARPROC ll = GetProcAddress(GetModuleHandleW(L"kernel32.dll"), "LoadLibraryW");
    if (!QueueUserAPC((PAPCFUNC)(void *)ll, thread, (ULONG_PTR)rp)) {
        VirtualFreeEx(proc, rp, 0, MEM_RELEASE);
        return -1;
    }
    return 0;   /* the path buffer stays: LoadLibraryW reads it later */
}

static int inject_other_arch(DWORD pid)
{
    wchar_t cmd[MAX_PATH * 2];
#ifdef _WIN64
    const wchar_t *helper = L"nvinject32.exe";
#else
    const wchar_t *helper = L"nvinject.exe";
#endif
    _snwprintf(cmd, MAX_PATH * 2 - 1, L"\"%ls\\%ls\" -p %lu", self_dir, helper,
               (unsigned long)pid);
    cmd[MAX_PATH * 2 - 1] = 0;
    STARTUPINFOW si;
    PROCESS_INFORMATION pi;
    memset(&si, 0, sizeof(si));
    si.cb = sizeof(si);
    if (!CreateProcessW(NULL, cmd, NULL, NULL, FALSE, CREATE_NO_WINDOW, NULL, NULL, &si, &pi))
        return -1;
    WaitForSingleObject(pi.hProcess, 65000);
    DWORD code = 1;
    GetExitCodeProcess(pi.hProcess, &code);
    CloseHandle(pi.hThread);
    CloseHandle(pi.hProcess);
    return code == 0 ? 0 : -1;
}

/* CreateProcessInternalW(hToken, app, cmdline, psa, tsa, inherit, flags, env,
 * cwd, si, pi, hNewToken): force CREATE_SUSPENDED, inject, resume. */
static uintptr_t h_createprocess(nvt_hook *h, uintptr_t *args, void *ret)
{
    (void)ret;
    tstate *t = ts_get();
    uintptr_t a[NVT_MAX_ARGS];
    memcpy(a, args, h->nargs * sizeof(uintptr_t));
    DWORD flags = (DWORD)a[6];
    if (flags & (DEBUG_PROCESS | DEBUG_ONLY_THIS_PROCESS))
        return nvt_call_orig(h->orig, args, h->nargs);   /* a debugger owns the child */
    int was_suspended = (flags & CREATE_SUSPENDED) != 0;
    a[6] = flags | CREATE_SUSPENDED;
    uintptr_t r = nvt_call_orig(h->orig, a, h->nargs);
    DWORD le = GetLastError();
    if (!r || !t)
        return r;
    t->busy = 1;
    PROCESS_INFORMATION pi;
    if (safe_read(&pi, a[10], sizeof(pi))) {
        BOOL wow_child = FALSE, wow_self = FALSE;
        IsWow64Process(pi.hProcess, &wow_child);
        IsWow64Process(GetCurrentProcess(), &wow_self);
#ifdef _WIN64
        int same = !wow_child;
#else
        int same = wow_self ? wow_child : 1;
#endif
        int rc = same ? inject_same_arch(pi.hProcess, pi.hThread) : inject_other_arch(pi.dwProcessId);
        nvt_note(rc ? 1 : 0, "child pid %lu: %s (%s)", (unsigned long)pi.dwProcessId,
                 rc ? "injection failed" : same ? "load queued as APC" : "injected",
                 same ? "same arch" : "other arch");
        if (!was_suspended)
            ResumeThread(pi.hThread);
    }
    t->busy = 0;
    SetLastError(le);
    return r;
}

/* ------------------------------------------------------------------ install */

static void emit_string(uint32_t id, const char *s)
{
    nvt_buf b = {0};
    nvt_rec_begin(&b, NTR_REC_STRING);
    nvt_buf_put(&b, &id, 4);
    nvt_buf_put(&b, s, strlen(s) + 1);
    nvt_rec_end(&b);
    nvt_buf_free(&b);
}

static void emit_hook(const nvt_hook *h, uint32_t status)
{
    nvt_buf b = {0};
    ntr_hook r;
    memset(&r, 0, sizeof(r));
    r.hook_id = h->id;
    r.flags = h->hookflags | (status ? 0 : NTR_HOOK_OK);
    r.syscall_nr = h->syscall_nr;
    r.nargs = h->nargs;
    r.address = (uintptr_t)h->target;
    r.arg0_size = h->fd ? h->fd->arg0size : 0;
    r.status = status;
    nvt_rec_begin(&b, NTR_REC_HOOK);
    nvt_buf_put(&b, &r, sizeof(r));
    nvt_buf_put(&b, h->module, strlen(h->module) + 1);
    nvt_buf_put(&b, h->name, strlen(h->name) + 1);
    nvt_rec_end(&b);
    nvt_buf_free(&b);
}

static int list_has(const char *list, const char *name, int prefix)
{
    size_t n = strlen(name);
    const char *e = list;
    while (*e) {
        while (*e == ',' || *e == ' ')
            e++;
        const char *comma = strchr(e, ',');
        size_t len = comma ? (size_t)(comma - e) : strlen(e);
        while (len && e[len - 1] == ' ')
            len--;
        if (len && (prefix ? len <= n : len == n) && !_strnicmp(e, name, len))
            return 1;
        if (!comma)
            break;
        e = comma + 1;
    }
    return 0;
}

static const char *kmt_suffix(const char *name)
{
    if (!strncmp(name, "NtGdiDdDDI", 10))
        return name + 10;
    if (!strncmp(name, "NtDxgk", 6))
        return name + 6;
    return NULL;
}

static const nvt_fdesc *find_fdesc(const char *name)
{
    for (size_t i = 0; i < NFDESCS; i++)
        if (!strcmp(nvt_fdescs[i].name, name))
            return &nvt_fdescs[i];
    /* NtDxgk<X> shares the layout of NtGdiDdDDI<X> */
    const char *sfx = kmt_suffix(name);
    if (sfx && !strncmp(name, "NtDxgk", 6))
        for (size_t i = 0; i < NFDESCS; i++)
            if (!strncmp(nvt_fdescs[i].name, "NtGdiDdDDI", 10) && !strcmp(nvt_fdescs[i].name + 10, sfx))
                return &nvt_fdescs[i];
    return NULL;
}

static uint32_t gk_for(const char *name)
{
    static const struct {
        const char *n;
        uint32_t gk;
    } tab[] = {
        {"Lock", GK_LOCK}, {"Lock2", GK_LOCK2}, {"Unlock", GK_UNLOCK}, {"Unlock2", GK_UNLOCK2},
        {"CreateAllocation", GK_CREATEALLOC}, {"CreateAllocation2", GK_CREATEALLOC2},
        {"DestroyAllocation", GK_DESTROYALLOC}, {"DestroyAllocation2", GK_DESTROYALLOC},
        {"MapGpuVirtualAddress", GK_MAPGPUVA}, {"UpdateGpuVirtualAddress", GK_UPDATEGPUVA},
        {"FreeGpuVirtualAddress", GK_FREEGPUVA}, {"SubmitCommand", GK_SUBMIT},
        {"SubmitCommandToHwQueue", GK_SUBMITHWQ}, {"CreateDoorbell", GK_CREATEDOORBELL},
        {"DestroyDoorbell", GK_DESTROYDOORBELL}, {"NotifyWorkSubmission", GK_NOTIFYWORK},
        {"CreateHwQueue", GK_CREATEHWQUEUE}, {"CreatePagingQueue", GK_CREATEPAGINGQUEUE},
        {"CreateSynchronizationObject2", GK_CREATESYNC2}, {"CreateNativeFence", GK_CREATENATIVEFENCE},
        {"OpenNativeFenceFromNtHandle", GK_OPENNATIVEFENCE},
        {"OpenSyncObjectFromNtHandle2", GK_OPENSYNC2}, {"OpenResource2", GK_OPENRESOURCE},
        {"OpenResourceFromNtHandle", GK_OPENRESOURCENT}, {"Escape", GK_ESCAPE},
        {"SignalSynchronizationObjectFromGpu", GK_SYNCPOINT},
        {"SignalSynchronizationObjectFromGpu2", GK_SYNCPOINT},
        {"WaitForSynchronizationObjectFromGpu", GK_SYNCPOINT},
        {"SubmitSignalSyncObjectsToHwQueue", GK_SYNCPOINT},
        {"SubmitWaitForSyncObjectsToHwQueue", GK_SYNCPOINT}, {"Present", GK_SYNCPOINT},
        {"PresentMultiPlaneOverlay", GK_SYNCPOINT}, {"PresentMultiPlaneOverlay2", GK_SYNCPOINT},
        {"PresentMultiPlaneOverlay3", GK_SYNCPOINT}, {"SubmitPresentToHwQueue", GK_SYNCPOINT},
        {"SubmitPresentBltToHwQueue", GK_SYNCPOINT}, {"Render", GK_SYNCPOINT},
    };
    if (!strcmp(name, "NtDeviceIoControlFile") || !strcmp(name, "NtFsControlFile"))
        return GK_IOCTL;
    if (!strcmp(name, "NtReadFile"))
        return GK_IOREAD;
    if (!strcmp(name, "NtWriteFile"))
        return GK_IOWRITE;
    const char *sfx = kmt_suffix(name);
    if (!sfx)
        return GK_NONE;
    for (size_t i = 0; i < sizeof(tab) / sizeof(tab[0]); i++)
        if (!strcmp(tab[i].n, sfx))
            return tab[i].gk;
    return GK_NONE;
}

/* Hooks prepared in one batch, committed with one thread freeze. */
typedef struct batch {
    nvt_hook *h[MAX_HOOKS];
    uint32_t err[MAX_HOOKS];
    int n;
} batch;

static nvt_hook *hook_new(const char *modname, const char *name, void *addr, const nvt_fdesc *fd,
                          nvt_handler handler, uint32_t extra_flags, uint32_t nargs_hint)
{
    LONG idx = InterlockedIncrement(&nhooks) - 1;
    if (idx >= MAX_HOOKS)
        return NULL;
    nvt_hook *h = &hooks[idx];
    memset(h, 0, sizeof(*h));
    h->id = (uint32_t)idx + 1;
    h->target = addr;
    h->fd = fd;
    h->handler = handler;
    h->flags = (fd ? fd->flags : NVT_FN_RAW) | extra_flags;
    h->hookflags = fd ? NTR_HOOK_LAYOUT : (handler ? 0 : NTR_HOOK_RAW);
    lstrcpynA(h->name, name, sizeof(h->name));
    lstrcpynA(h->module, modname, sizeof(h->module));
    h->gk = gk_for(name);
    uint8_t head[16];
    h->syscall_nr = (addr && nvt_safe_copy(head, addr, 16) == 16) ? nvt_stub_syscall_nr(head) : 0xFFFFFFFFu;
    if (h->syscall_nr != 0xFFFFFFFFu)
        h->hookflags |= NTR_HOOK_SYSCALL_STUB;
    h->nargs = fd ? fd->nargs : nargs_hint;
    return h;
}

/* validate and prepare one hook into the batch; failures are logged */
static void hook_add(batch *bt, const char *modname, const char *name, void *addr,
                     const nvt_fdesc *fd, nvt_handler handler, uint32_t extra_flags,
                     uint32_t nargs_hint)
{
    nvt_hook *h = hook_new(modname, name, addr, fd, handler, extra_flags, nargs_hint);
    if (!h)
        return;
    if (!addr) {
        emit_hook(h, NTR_HOOKERR_NOTFOUND);
        return;
    }
    if (list_has(nvt_cfg.exclude, name, 0)) {
        emit_hook(h, NTR_HOOKERR_EXCLUDED);
        return;
    }
#ifndef _WIN64
    uint32_t ab;
    uint8_t code[64];
    if (h->syscall_nr != 0xFFFFFFFFu && nvt_safe_copy(code, addr, sizeof(code)) == sizeof(code) &&
        !nvt_stub_argbytes(code, &ab)) {
        h->argbytes = ab;
        if (!fd || fd->nargs != ab / 4)
            h->nargs = ab / 4;
    } else if (nargs_hint || fd) {
        h->argbytes = h->nargs * 4;
    } else {
        emit_hook(h, NTR_HOOKERR_DECODE);
        return;
    }
#endif
    if (h->nargs > NVT_MAX_ARGS) {
        emit_hook(h, NTR_HOOKERR_DECODE);
        return;
    }
    uint32_t err = 0;
    if (nvt_hook_prepare(h, &err)) {
        emit_hook(h, err ? err : NTR_HOOKERR_DECODE);
        return;
    }
    bt->h[bt->n++] = h;
}

static void batch_commit(batch *bt)
{
    uint32_t moved = 0;
    if (!bt->n)
        return;
    int ok = nvt_hooks_commit(bt->h, bt->n, bt->err, &moved);
    for (int i = 0; i < bt->n; i++)
        emit_hook(bt->h[i], bt->err[i]);
    if (moved || ok != bt->n)
        nvt_note(moved ? 0 : 1, "hook batch: %d of %d installed, %u threads moved out of patched code",
                 ok, bt->n, moved);
    bt->n = 0;
}

static void hook_module_table(batch *bt, HMODULE mod, const char *modname)
{
    for (size_t i = 0; i < NFDESCS; i++) {
        if (_stricmp(nvt_fdescs[i].module, modname))
            continue;
        if (!strncmp(nvt_fdescs[i].name, "NtGdiDdDDI", 10))
            continue;                   /* done by the export scan */
        void *a = (void *)GetProcAddress(mod, nvt_fdescs[i].name);
        if (!a)
            continue;                   /* e.g. NtUnmapViewOfSectionEx on old builds */
        hook_add(bt, modname, nvt_fdescs[i].name, a, &nvt_fdescs[i], NULL, 0, 0);
    }
}

/* every NtGdiDdDDI* and NtDxgk* export of win32u, known or not */
static void hook_win32u(batch *bt, HMODULE mod)
{
    uint8_t *base = (uint8_t *)mod;
    IMAGE_DOS_HEADER *dos = (IMAGE_DOS_HEADER *)base;
    IMAGE_NT_HEADERS *nt = (IMAGE_NT_HEADERS *)(base + dos->e_lfanew);
    IMAGE_DATA_DIRECTORY *dd = &nt->OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_EXPORT];
    if (!dd->VirtualAddress)
        return;
    IMAGE_EXPORT_DIRECTORY *ex = (IMAGE_EXPORT_DIRECTORY *)(base + dd->VirtualAddress);
    DWORD *names = (DWORD *)(base + ex->AddressOfNames);
    WORD *ords = (WORD *)(base + ex->AddressOfNameOrdinals);
    DWORD *funcs = (DWORD *)(base + ex->AddressOfFunctions);
    for (DWORD i = 0; i < ex->NumberOfNames && bt->n < MAX_HOOKS - 32; i++) {
        const char *name = (const char *)(base + names[i]);
        if (!kmt_suffix(name))
            continue;
        if (ords[i] >= ex->NumberOfFunctions)
            continue;
        DWORD rva = funcs[ords[i]];
        if (rva >= dd->VirtualAddress && rva < dd->VirtualAddress + dd->Size)
            continue;                   /* forwarder */
        const nvt_fdesc *fd = find_fdesc(name);
        hook_add(bt, "win32u.dll", name, base + rva, fd, NULL, 0, fd ? 0 : (P == 8 ? NVT_MAX_ARGS : 0));
    }
}

static batch *batch_alloc(void)
{
    batch *bt = VirtualAlloc(NULL, sizeof(batch), MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE);
    if (bt)
        bt->n = 0;
    return bt;
}

/* Hooks each module once. win32u may arrive late (from the DLL load
 * notification, which passes its base: the loader cannot look it up yet). */
static void maybe_hook_modules(HMODULE win32u_base)
{
    batch *bt = batch_alloc();
    HMODULE m;
    if (!bt)
        return;
    if (nvt_cfg.ntdll && (m = GetModuleHandleW(L"ntdll.dll")) &&
        !InterlockedCompareExchange(&ntdll_hooked, 1, 0))
        hook_module_table(bt, m, "ntdll.dll");
    m = win32u_base ? win32u_base : GetModuleHandleW(L"win32u.dll");
    if (m && !InterlockedCompareExchange(&win32u_hooked, 1, 0))
        hook_win32u(bt, m);
    if (nvt_cfg.follow && (m = GetModuleHandleW(L"kernelbase.dll")) &&
        !InterlockedCompareExchange(&kernelbase_hooked, 1, 0)) {
        void *a = (void *)GetProcAddress(m, "CreateProcessInternalW");
        hook_add(bt, "kernelbase.dll", "CreateProcessInternalW", a, NULL, h_createprocess,
                 NVT_FN_PROCESS, 12);
    }
    batch_commit(bt);
    VirtualFree(bt, 0, MEM_RELEASE);
}

/* ------------------------------------------------------------------ modules */

static void module_version(uintptr_t base, uint16_t ver[4], uint32_t *timestamp)
{
    IMAGE_DOS_HEADER dos;
    IMAGE_NT_HEADERS nt;
    memset(ver, 0, 8);
    *timestamp = 0;
    if (!safe_read(&dos, base, sizeof(dos)) || dos.e_magic != IMAGE_DOS_SIGNATURE)
        return;
    if (!safe_read(&nt, base + dos.e_lfanew, sizeof(nt)) || nt.Signature != IMAGE_NT_SIGNATURE)
        return;
    *timestamp = nt.FileHeader.TimeDateStamp;
    IMAGE_DATA_DIRECTORY dd = nt.OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_RESOURCE];
    if (!dd.VirtualAddress)
        return;
    uintptr_t rsrc = base + dd.VirtualAddress;
    /* level 1: type RT_VERSION (16); level 2: first name; level 3: first language */
    uintptr_t dir = rsrc;
    uint32_t want = 16;
    for (int level = 0; level < 3; level++) {
        IMAGE_RESOURCE_DIRECTORY d;
        if (!safe_read(&d, dir, sizeof(d)))
            return;
        uint32_t n = d.NumberOfNamedEntries + d.NumberOfIdEntries;
        if (n > 4096)
            return;
        IMAGE_RESOURCE_DIRECTORY_ENTRY e;
        int found = 0;
        for (uint32_t i = 0; i < n; i++) {
            if (!safe_read(&e, dir + sizeof(d) + i * sizeof(e), sizeof(e)))
                return;
            if (level == 0 && (e.Name & 0x80000000u))
                continue;
            if (level == 0 && (e.Name & 0xFFFF) != want)
                continue;
            found = 1;
            break;
        }
        if (!found)
            return;
        if (level < 2) {
            if (!(e.OffsetToData & 0x80000000u))
                return;
            dir = rsrc + (e.OffsetToData & 0x7FFFFFFFu);
        } else {
            IMAGE_RESOURCE_DATA_ENTRY de;
            if (!safe_read(&de, rsrc + e.OffsetToData, sizeof(de)))
                return;
            uint8_t vi[512];
            size_t got = nvt_safe_copy(vi, (const void *)(base + de.OffsetToData),
                                       de.Size < sizeof(vi) ? de.Size : sizeof(vi));
            for (size_t o = 0; o + 52 <= got; o += 4) {
                uint32_t sig;
                memcpy(&sig, vi + o, 4);
                if (sig == 0xFEEF04BDu) {
                    uint32_t ms, ls;
                    memcpy(&ms, vi + o + 8, 4);
                    memcpy(&ls, vi + o + 12, 4);
                    ver[0] = (uint16_t)(ms >> 16);
                    ver[1] = (uint16_t)ms;
                    ver[2] = (uint16_t)(ls >> 16);
                    ver[3] = (uint16_t)ls;
                    return;
                }
            }
        }
    }
}

/* module record built on the stack (also used at process exit, where the
 * heap is not safe) */
static void emit_module(uintptr_t base, uint64_t size, const wchar_t *path, size_t pathlen,
                        uint32_t flags)
{
    uint8_t store[sizeof(ntr_rec_hdr) + sizeof(ntr_module) + MAX_PATH * 3 + 16];
    nvt_buf b = {store, 0, sizeof(store), 0, 1};
    ntr_module m;
    char u8[MAX_PATH * 3];
    memset(&m, 0, sizeof(m));
    m.base = base;
    m.size = size;
    m.flags = flags;
    module_version(base, m.version, &m.timestamp);
    int n = WideCharToMultiByte(CP_UTF8, 0, path, (int)pathlen, u8, sizeof(u8) - 1, NULL, NULL);
    if (n < 0)
        n = 0;
    u8[n] = 0;
    nvt_rec_begin(&b, NTR_REC_MODULE);
    nvt_buf_put(&b, &m, sizeof(m));
    nvt_buf_put(&b, u8, (size_t)n + 1);
    nvt_rec_end(&b);
}

/* minimal loader structures (stable since NT 5) */
typedef struct nvt_unicode {
    USHORT Length, MaximumLength;
    PWSTR Buffer;
} nvt_unicode;

typedef struct nvt_ldr_entry {
    LIST_ENTRY InLoadOrderLinks;
    LIST_ENTRY InMemoryOrderLinks;
    LIST_ENTRY InInitializationOrderLinks;
    PVOID DllBase;
    PVOID EntryPoint;
    ULONG SizeOfImage;
    nvt_unicode FullDllName;
    nvt_unicode BaseDllName;
} nvt_ldr_entry;

typedef struct nvt_peb_ldr {
    ULONG Length;
    BOOLEAN Initialized;
    PVOID SsHandle;
    LIST_ENTRY InLoadOrderModuleList;
} nvt_peb_ldr;

static nvt_peb_ldr *peb_ldr(void)
{
    uint8_t *peb;
#ifdef _WIN64
    __asm__ volatile("movq %%gs:0x60, %0" : "=r"(peb));
    return *(nvt_peb_ldr **)(peb + 0x18);
#else
    __asm__ volatile("movl %%fs:0x30, %0" : "=r"(peb));
    return *(nvt_peb_ldr **)(peb + 0x0C);
#endif
}

/* Walks the loader list. The caller holds the loader lock (DllMain, loader
 * notification) or the other threads are gone (exit). */
static void snapshot_modules(uint32_t flags)
{
    nvt_peb_ldr *ldr = peb_ldr();
    if (!ldr)
        return;
    LIST_ENTRY *head = &ldr->InLoadOrderModuleList;
    int guard = 0;
    for (LIST_ENTRY *e = head->Flink; e != head && guard < 4096; e = e->Flink, guard++) {
        nvt_ldr_entry *m = CONTAINING_RECORD(e, nvt_ldr_entry, InLoadOrderLinks);
        emit_module((uintptr_t)m->DllBase, m->SizeOfImage, m->FullDllName.Buffer,
                    m->FullDllName.Length / 2, flags);
        if (flags == NTR_MOD_INITIAL && m->BaseDllName.Buffer)
            nvmod_update(m->BaseDllName.Buffer, m->BaseDllName.Length / 2, (uintptr_t)m->DllBase,
                         m->SizeOfImage, 1);
    }
}

/* first loaded module whose base name starts with a trigger prefix */
static int trigger_loaded(char *which, size_t whichsz)
{
    nvt_peb_ldr *ldr = peb_ldr();
    if (!ldr)
        return 0;
    LIST_ENTRY *head = &ldr->InLoadOrderModuleList;
    int guard = 0;
    for (LIST_ENTRY *e = head->Flink; e != head && guard < 4096; e = e->Flink, guard++) {
        nvt_ldr_entry *m = CONTAINING_RECORD(e, nvt_ldr_entry, InLoadOrderLinks);
        char nm[64];
        int n = WideCharToMultiByte(CP_UTF8, 0, m->BaseDllName.Buffer, m->BaseDllName.Length / 2, nm,
                                    sizeof(nm) - 1, NULL, NULL);
        if (n <= 0)
            continue;
        nm[n] = 0;
        if (list_has(nvt_cfg.triggers, nm, 1)) {
            lstrcpynA(which, nm, (int)whichsz);
            return 1;
        }
    }
    return 0;
}

typedef struct nvt_dll_notif {
    ULONG Flags;
    const nvt_unicode *FullDllName;
    const nvt_unicode *BaseDllName;
    PVOID DllBase;
    ULONG SizeOfImage;
} nvt_dll_notif;

static void full_start(const char *reason);

static VOID CALLBACK dll_notify(ULONG reason, const nvt_dll_notif *d, PVOID ctx)
{
    (void)ctx;
    if (nvt_dead || start_failed)
        return;
    DWORD le = GetLastError();
    char base[64] = "";
    if (d->BaseDllName && d->BaseDllName->Buffer) {
        int n = WideCharToMultiByte(CP_UTF8, 0, d->BaseDllName->Buffer, d->BaseDllName->Length / 2, base,
                                    sizeof(base) - 1, NULL, NULL);
        base[n > 0 ? n : 0] = 0;
    }
    if (!started) {
        /* lazy mode: a trigger module is being loaded (before its DllMain) */
        if (reason == 1 && list_has(nvt_cfg.triggers, base, 1)) {
            char why[96];
            _snprintf(why, sizeof(why) - 1, "trigger:%s", base);
            why[sizeof(why) - 1] = 0;
            full_start(why);
        }
        SetLastError(le);
        return;
    }
    if (!nvt_log_ready) {
        SetLastError(le);
        return;
    }
    tstate *t = ts_get();
    int was_busy = t ? t->busy : 0;
    if (t)
        t->busy = 1;
    if (d->BaseDllName && d->BaseDllName->Buffer)
        nvmod_update(d->BaseDllName->Buffer, d->BaseDllName->Length / 2, (uintptr_t)d->DllBase,
                     d->SizeOfImage, reason == 1);
    if (reason == 1) {
        emit_module((uintptr_t)d->DllBase, d->SizeOfImage, d->FullDllName->Buffer,
                    d->FullDllName->Length / 2, NTR_MOD_LOADED);
        if (!_stricmp(base, "win32u.dll"))
            maybe_hook_modules((HMODULE)d->DllBase);
    } else if (reason == 2) {
        nvt_buf b = {0};
        char u8[MAX_PATH * 3];
        uint64_t mbase = (uintptr_t)d->DllBase;
        int n = WideCharToMultiByte(CP_UTF8, 0, d->FullDllName->Buffer,
                                    d->FullDllName->Length / 2, u8, sizeof(u8) - 1, NULL, NULL);
        if (n < 0)
            n = 0;
        u8[n] = 0;
        nvt_rec_begin(&b, NTR_REC_MODULE_UNLOAD);
        nvt_buf_put(&b, &mbase, 8);
        nvt_buf_put(&b, u8, (size_t)n + 1);
        nvt_rec_end(&b);
        nvt_buf_free(&b);
    }
    if (t)
        t->busy = was_busy;
    SetLastError(le);
}

/* ------------------------------------------------------------------ config */

/* Settings come from (first found wins): <dll dir>\nvtrace-<pid>.cfg (written
 * by nvinject -p/--dwm/--watch for one injection), the environment, then
 * <dll dir>\nvtrace.cfg. Files hold NAME=value lines (NVTRACE_ optional). */
static char *cfg_pid_text, *cfg_base_text;

static char *read_text_file(const wchar_t *path)
{
    HANDLE f = CreateFileW(path, GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                           NULL, OPEN_EXISTING, 0, NULL);
    if (f == INVALID_HANDLE_VALUE)
        return NULL;
    char *buf = VirtualAlloc(NULL, 16384, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE);
    DWORD rd = 0;
    if (buf && (!ReadFile(f, buf, 16383, &rd, NULL) || !rd)) {
        VirtualFree(buf, 0, MEM_RELEASE);
        buf = NULL;
    }
    if (buf)
        buf[rd] = 0;
    CloseHandle(f);
    return buf;
}

static int cfg_text_get(const char *text, const char *name, char *out, size_t outsz)
{
    const char *short_name = !strncmp(name, "NVTRACE_", 8) ? name + 8 : name;
    for (const char *l = text; l && *l;) {
        const char *e = strpbrk(l, "\r\n");
        size_t len = e ? (size_t)(e - l) : strlen(l);
        while (len && (*l == ' ' || *l == '\t')) {
            l++;
            len--;
        }
        const char *eq = memchr(l, '=', len);
        if (eq && *l != '#' && *l != ';') {
            size_t kl = (size_t)(eq - l);
            while (kl && (l[kl - 1] == ' ' || l[kl - 1] == '\t'))
                kl--;
            if ((kl == strlen(name) && !_strnicmp(l, name, kl)) ||
                (kl == strlen(short_name) && !_strnicmp(l, short_name, kl))) {
                const char *v = eq + 1;
                size_t vl = len - (size_t)(v - l);
                while (vl && (*v == ' ' || *v == '\t')) {
                    v++;
                    vl--;
                }
                while (vl && (v[vl - 1] == ' ' || v[vl - 1] == '\t'))
                    vl--;
                if (vl >= outsz)
                    vl = outsz - 1;
                memcpy(out, v, vl);
                out[vl] = 0;
                return 1;
            }
        }
        if (!e)
            break;
        l = e + 1;
        while (*l == '\r' || *l == '\n')
            l++;
    }
    return 0;
}

/* NVTRACE_<name> as UTF-8; 1 if set */
static int cfg_get(const char *name, char *out, size_t outsz)
{
    wchar_t wname[64], wv[MAX_PATH];
    if (cfg_pid_text && cfg_text_get(cfg_pid_text, name, out, outsz))
        return 1;
    MultiByteToWideChar(CP_UTF8, 0, name, -1, wname, 64);
    DWORD n = GetEnvironmentVariableW(wname, wv, MAX_PATH);
    if (n && n < MAX_PATH) {
        WideCharToMultiByte(CP_UTF8, 0, wv, -1, out, (int)outsz, NULL, NULL);
        out[outsz - 1] = 0;
        return 1;
    }
    if (cfg_base_text && cfg_text_get(cfg_base_text, name, out, outsz))
        return 1;
    return 0;
}

static uint64_t cfg_u64(const char *name, uint64_t def)
{
    char v[64];
    if (!cfg_get(name, v, sizeof(v)) || !v[0])
        return def;
    return _strtoui64(v, NULL, 0);
}

static void load_config(void)
{
    char v[MAX_PATH * 3];
    wchar_t path[MAX_PATH + 32];
    _snwprintf(path, MAX_PATH + 31, L"%ls\\nvtrace-%lu.cfg", self_dir, (unsigned long)GetCurrentProcessId());
    path[MAX_PATH + 31] = 0;
    cfg_pid_text = read_text_file(path);
    _snwprintf(path, MAX_PATH + 31, L"%ls\\nvtrace.cfg", self_dir);
    path[MAX_PATH + 31] = 0;
    cfg_base_text = read_text_file(path);

    if (cfg_get("NVTRACE_DIR", v, sizeof(v)) && v[0])
        MultiByteToWideChar(CP_UTF8, 0, v, -1, nvt_cfg.dir, MAX_PATH);
    else
        lstrcpyW(nvt_cfg.dir, L"C:\\nvtrace");
    nvt_cfg.dir[MAX_PATH - 1] = 0;
    nvt_cfg.rawsize = (uint32_t)cfg_u64("NVTRACE_RAWSIZE", 4096);
    if (nvt_cfg.rawsize > (1u << 20))
        nvt_cfg.rawsize = 1u << 20;
    nvt_cfg.maxbuf = cfg_u64("NVTRACE_MAXBUF", sizeof(void *) == 8 ? (64ull << 20) : (32ull << 20));
    if (nvt_cfg.maxbuf > (sizeof(void *) == 8 ? (1ull << 30) : (256ull << 20)))
        nvt_cfg.maxbuf = sizeof(void *) == 8 ? (1ull << 30) : (256ull << 20);
    nvt_cfg.stack = (uint32_t)cfg_u64("NVTRACE_STACK", 8);
    if (nvt_cfg.stack > 48)
        nvt_cfg.stack = 48;
    nvt_cfg.files = 1;
    if (cfg_get("NVTRACE_FILES", v, sizeof(v)) && v[0])
        nvt_cfg.files = !_stricmp(v, "all") ? 2 : !_stricmp(v, "nv") ? 1 : (int)strtoul(v, NULL, 0);
    nvt_cfg.maps = (int)cfg_u64("NVTRACE_MAPS", 1);
    nvt_cfg.follow = (int)cfg_u64("NVTRACE_FOLLOW", 0);
    nvt_cfg.chase = (uint32_t)cfg_u64("NVTRACE_CHASE", 16384);
    if (nvt_cfg.chase > (1u << 20))
        nvt_cfg.chase = 1u << 20;
    nvt_cfg.chase_depth = (uint32_t)cfg_u64("NVTRACE_CHASE_DEPTH", 2);
    if (nvt_cfg.chase_depth > 4)
        nvt_cfg.chase_depth = 4;
    nvt_cfg.chase_max = (uint32_t)cfg_u64("NVTRACE_CHASE_MAX", 16);
    if (nvt_cfg.chase_max > 64)
        nvt_cfg.chase_max = 64;
    nvt_cfg.cmdbuf = (uint32_t)cfg_u64("NVTRACE_CMDBUF", 64u << 20);
    nvt_cfg.maxrec = cfg_u64("NVTRACE_MAXREC", sizeof(void *) == 8 ? (256ull << 20) : (32ull << 20));
    if (nvt_cfg.maxrec < nvt_cfg.maxbuf)
        nvt_cfg.maxrec = nvt_cfg.maxbuf;
    if (nvt_cfg.maxrec > 0xF0000000ull)
        nvt_cfg.maxrec = 0xF0000000ull;
    nvt_cfg.ntdll = (int)cfg_u64("NVTRACE_NTDLL", 1);
    nvt_cfg.devices_all = cfg_get("NVTRACE_DEVICES", v, sizeof(v)) && !_stricmp(v, "all");
    nvt_cfg.exclude[0] = 0;
    if (cfg_get("NVTRACE_EXCLUDE", v, sizeof(v)))
        lstrcpynA(nvt_cfg.exclude, v, sizeof(nvt_cfg.exclude));
    nvt_cfg.lazy = cfg_get("NVTRACE_MODE", v, sizeof(v)) && !_stricmp(v, "lazy");
    if (cfg_get("NVTRACE_TRIGGER", v, sizeof(v)) && v[0])
        lstrcpynA(nvt_cfg.triggers, v, sizeof(nvt_cfg.triggers));
    else
        lstrcpynA(nvt_cfg.triggers,
                  "nvldumd,nvwgf2um,nvd3dum,nvoglv,nvcuda,nvapi,nvml,nvencodeapi,nvcuvid,nvofapi,"
                  "_nvngx,nvngx",
                  sizeof(nvt_cfg.triggers));
    nvt_cfg.maxlog = cfg_u64("NVTRACE_MAXLOG", 16ull << 30);
    nvt_cfg.pushbuf = (uint32_t)cfg_u64("NVTRACE_PUSHBUF", sizeof(void *) == 8 ? (64u << 20) : (16u << 20));
    nvt_cfg.gpfifo = (int)cfg_u64("NVTRACE_GPFIFO", 1);
    nvt_cfg.snap = (int)cfg_u64("NVTRACE_SNAP", 1);
    nvt_cfg.snap_ms = (uint32_t)cfg_u64("NVTRACE_SNAP_MS", 2);
    if (nvt_cfg.snap_ms < 1)
        nvt_cfg.snap_ms = 1;
    nvt_cfg.snap_us = (uint32_t)cfg_u64("NVTRACE_SNAP_US", 200);
    if (nvt_cfg.snap_us > 100000)
        nvt_cfg.snap_us = 100000;
    nvt_cfg.snap_region = cfg_u64("NVTRACE_SNAP_REGION", 64u << 20);
    nvt_cfg.snap_submit = (int)cfg_u64("NVTRACE_SNAP_SUBMIT", 1);
    nvt_cfg.snap_submit_us = (uint32_t)cfg_u64("NVTRACE_SNAP_SUBMIT_US", 500);
    nvt_cfg.reg = 1;
    nvt_cfg.reg_match[0] = 0;
    if (cfg_get("NVTRACE_REG", v, sizeof(v)) && v[0]) {
        if (!_stricmp(v, "all"))
            nvt_cfg.reg = 2;
        else if (!strcmp(v, "0") || !strcmp(v, "1"))
            nvt_cfg.reg = v[0] - '0';
        else {
            for (char *q = v; *q; q++)
                if (*q >= 'A' && *q <= 'Z')
                    *q = (char)(*q - 'A' + 'a');
            lstrcpynA(nvt_cfg.reg_match, v, sizeof(nvt_cfg.reg_match));
        }
    }
    nvt_cfg.watch[0] = 0;
    if (cfg_get("NVTRACE_WATCH", v, sizeof(v)))
        lstrcpynA(nvt_cfg.watch, v, sizeof(nvt_cfg.watch));
    nvt_cfg.iopending = (int)cfg_u64("NVTRACE_IOPENDING", 1);
    nvt_cfg.probe = (int)cfg_u64("NVTRACE_PROBE", 1);
    nvt_cfg.regions = (int)cfg_u64("NVTRACE_REGIONS", 1);
    nvt_cfg.selftest = (int)cfg_u64("NVTRACE_SELFTEST", 0);
}

static void mkdirs(const wchar_t *dir)
{
    wchar_t tmp[MAX_PATH];
    lstrcpynW(tmp, dir, MAX_PATH);
    for (wchar_t *p = tmp + 3; *p; p++) {
        if (*p == L'\\' || *p == L'/') {
            wchar_t c = *p;
            *p = 0;
            CreateDirectoryW(tmp, NULL);
            *p = c;
        }
    }
    CreateDirectoryW(tmp, NULL);
}

static wchar_t log_path[MAX_PATH * 2];

static int open_log(void)
{
    wchar_t exe[MAX_PATH];
    GetModuleFileNameW(NULL, exe, MAX_PATH);
    exe[MAX_PATH - 1] = 0;
    wchar_t *base = wcsrchr(exe, L'\\');
    base = base ? base + 1 : exe;
    wchar_t *dot = wcsrchr(base, L'.');
    if (dot)
        *dot = 0;
    mkdirs(nvt_cfg.dir);
    for (int i = 0; i < 100; i++) {
        if (i)
            _snwprintf(log_path, MAX_PATH * 2 - 1, L"%ls\\%ls-%lu-%d.ntr", nvt_cfg.dir, base,
                       (unsigned long)GetCurrentProcessId(), i);
        else
            _snwprintf(log_path, MAX_PATH * 2 - 1, L"%ls\\%ls-%lu.ntr", nvt_cfg.dir, base,
                       (unsigned long)GetCurrentProcessId());
        log_path[MAX_PATH * 2 - 1] = 0;
        if (!nvt_log_open(log_path, P, nvt_cfg.maxlog))
            return 0;
        if (GetLastError() != ERROR_FILE_EXISTS)
            return -1;
    }
    return -1;
}

typedef LONG(WINAPI *RtlGetVersion_t)(OSVERSIONINFOW *);
typedef LONG(WINAPI *NtQIP_t)(HANDLE, ULONG, PVOID, ULONG, PULONG);

static void kv(nvt_buf *b, const char *k, const char *v)
{
    nvt_buf_put(b, k, strlen(k));
    nvt_buf_put(b, "=", 1);
    nvt_buf_put(b, v, strlen(v) + 1);
}

static void kvw(nvt_buf *b, const char *k, const wchar_t *v)
{
    char u8[4096];
    int n = WideCharToMultiByte(CP_UTF8, 0, v, -1, u8, sizeof(u8) - 1, NULL, NULL);
    if (n <= 0)
        u8[0] = 0;
    u8[sizeof(u8) - 1] = 0;
    kv(b, k, u8);
}

static void kvu(nvt_buf *b, const char *k, unsigned long long v)
{
    char t[32];
    snprintf(t, sizeof(t), "%llu", v);
    kv(b, k, t);
}

static void emit_procinfo(void)
{
    nvt_buf b = {0};
    char t[256];
    wchar_t w[MAX_PATH];
    HMODULE ntdll = GetModuleHandleW(L"ntdll.dll");
    RtlGetVersion_t rgv = (RtlGetVersion_t)(void *)GetProcAddress(ntdll, "RtlGetVersion");
    NtQIP_t qip = (NtQIP_t)(void *)GetProcAddress(ntdll, "NtQueryInformationProcess");
    OSVERSIONINFOW ov;
    memset(&ov, 0, sizeof(ov));
    ov.dwOSVersionInfoSize = sizeof(ov);
    if (rgv)
        rgv(&ov);
    nvt_rec_begin(&b, NTR_REC_PROCINFO);
    snprintf(t, sizeof(t), "%lu.%lu.%lu", ov.dwMajorVersion, ov.dwMinorVersion, ov.dwBuildNumber);
    kv(&b, "os", t);
    DWORD ubr = 0, sz = sizeof(ubr);
    if (!RegGetValueW(HKEY_LOCAL_MACHINE, L"SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion",
                      L"UBR", RRF_RT_REG_DWORD, NULL, &ubr, &sz))
        kvu(&b, "os_ubr", ubr);
    kvu(&b, "ptr_size", P);
    kvu(&b, "pid", GetCurrentProcessId());
    if (qip) {
        ULONG_PTR pbi[6];
        if (!qip(GetCurrentProcess(), 0, pbi, sizeof(pbi), NULL))
            kvu(&b, "ppid", pbi[5]);
    }
    DWORD sess = 0;
    if (ProcessIdToSessionId(GetCurrentProcessId(), &sess))
        kvu(&b, "session", sess);
    BOOL wow = FALSE;
    IsWow64Process(GetCurrentProcess(), &wow);
    kv(&b, "wow64", wow ? "1" : "0");
    GetModuleFileNameW(NULL, w, MAX_PATH);
    w[MAX_PATH - 1] = 0;
    kvw(&b, "exe", w);
    kvw(&b, "cmdline", GetCommandLineW());
    GetCurrentDirectoryW(MAX_PATH, w);
    w[MAX_PATH - 1] = 0;
    kvw(&b, "cwd", w);
    kv(&b, "nvtrace", NVTRACE_VERSION);
    kvw(&b, "dll", self_path);
    kv(&b, "start_reason", start_reason);
    kv(&b, "config.source", cfg_pid_text ? "pid-file" : cfg_base_text ? "base-file+env" : "env");
    kvu(&b, "qpc_freq", (unsigned long long)nvt_qpc_freq.QuadPart);
    kvu(&b, "start_filetime", ((unsigned long long)start_ft.dwHighDateTime << 32) | start_ft.dwLowDateTime);
    LARGE_INTEGER q;
    QueryPerformanceCounter(&q);
    kvu(&b, "start_qpc", (unsigned long long)q.QuadPart);
    kvu(&b, "config.rawsize", nvt_cfg.rawsize);
    kvu(&b, "config.maxbuf", nvt_cfg.maxbuf);
    kvu(&b, "config.maxrec", nvt_cfg.maxrec);
    kvu(&b, "config.stack", nvt_cfg.stack);
    kv(&b, "config.devices", nvt_cfg.devices_all ? "all" : "nv");
    kv(&b, "config.files", nvt_cfg.files >= 2 ? "all" : nvt_cfg.files ? "nv" : "0");
    kv(&b, "config.reg", nvt_cfg.reg >= 2 ? "all" : !nvt_cfg.reg ? "0" : nvt_cfg.reg_match[0] ? nvt_cfg.reg_match : "nv");
    kvu(&b, "config.snap_submit", nvt_cfg.snap_submit);
    kvu(&b, "config.snap_submit_us", nvt_cfg.snap_submit_us);
    kvu(&b, "config.maps", nvt_cfg.maps);
    kvu(&b, "config.follow", nvt_cfg.follow);
    snprintf(t, sizeof(t), "%u/%u/%u", nvt_cfg.chase, nvt_cfg.chase_depth, nvt_cfg.chase_max);
    kv(&b, "config.chase", t);
    kvu(&b, "config.cmdbuf", nvt_cfg.cmdbuf);
    kv(&b, "config.exclude", nvt_cfg.exclude);
    kv(&b, "config.mode", nvt_cfg.lazy ? "lazy" : "full");
    kv(&b, "config.trigger", nvt_cfg.triggers);
    kvu(&b, "config.maxlog", nvt_cfg.maxlog);
    kvu(&b, "config.pushbuf", nvt_cfg.pushbuf);
    kvu(&b, "config.gpfifo", nvt_cfg.gpfifo);
    snprintf(t, sizeof(t), "%d/%ums/%uus/%llu", nvt_cfg.snap, nvt_cfg.snap_ms, nvt_cfg.snap_us,
             (unsigned long long)nvt_cfg.snap_region);
    kv(&b, "config.snap", t);
    kv(&b, "config.watch", nvt_cfg.watch);
    kvu(&b, "config.iopending", nvt_cfg.iopending);
    kvu(&b, "config.probe", nvt_cfg.probe);
    kvu(&b, "config.regions", nvt_cfg.regions);
    nvt_buf_put(&b, "", 1);
    nvt_rec_end(&b);
    nvt_buf_free(&b);
}

/* ------------------------------------------------------------------ aux thread */

static HANDLE aux_thread;

/* nvinject --stop writes <logdir>\nvtrace.stop (all processes) or
 * nvtrace-<pid>.stop; only files newer than our start count. */
static int stop_requested(void)
{
    wchar_t p[MAX_PATH + 40];
    WIN32_FILE_ATTRIBUTE_DATA fa;
    for (int i = 0; i < 2; i++) {
        if (i)
            _snwprintf(p, MAX_PATH + 39, L"%ls\\nvtrace-%lu.stop", nvt_cfg.dir,
                       (unsigned long)GetCurrentProcessId());
        else
            _snwprintf(p, MAX_PATH + 39, L"%ls\\nvtrace.stop", nvt_cfg.dir);
        p[MAX_PATH + 39] = 0;
        if (GetFileAttributesExW(p, GetFileExInfoStandard, &fa) &&
            CompareFileTime(&fa.ftLastWriteTime, &start_ft) > 0)
            return 1;
    }
    return 0;
}

static DWORD WINAPI aux_main(LPVOID arg)
{
    (void)arg;
    nvt_mark_thread_internal();
    ULONGLONG next_stop_check = 0;
    for (;;) {
        DWORD wait = 50;
        if (dev_pending_count())
            wait = 2;
        if (nvt_cfg.snap && gm_has_watch() && nvt_cfg.snap_ms < wait)
            wait = nvt_cfg.snap_ms;
        Sleep(wait);
        if (nvt_dead)
            break;
        dev_pending_poll(0);
        gm_sample_tick(0);
        ULONGLONG now = GetTickCount64();
        if (now >= next_stop_check) {
            next_stop_check = now + 250;
            if (stop_requested()) {
                nvt_note(0, "stop file found: log closed, hooks pass through");
                finish(0);
                break;
            }
        }
    }
    return 0;
}

/* ------------------------------------------------------------------ start */

typedef LONG(NTAPI *LdrRegisterDllNotification_t)(ULONG, PVOID, PVOID, PVOID *);

/* Open the log, describe the process, hook. Runs in DllMain (full mode) or
 * in the loader notification for a trigger module (lazy mode); both hold the
 * loader lock, which keeps the module list stable. */
static void full_start(const char *reason)
{
    if (InterlockedExchange(&started, 1))
        return;
    lstrcpynA(start_reason, reason, sizeof(start_reason));
    GetSystemTimeAsFileTime(&start_ft);
    if (open_log()) {
        InterlockedExchange(&start_failed, 1);
        return;
    }
    tstate *t = ts_get();
    if (t)
        t->busy = 1;
    emit_procinfo();
    for (uint32_t i = 0; i < NVT_NLABELS; i++)
        emit_string(i, nvt_labels[i]);
    for (uint32_t i = 0; i < LBL_COUNT - NVT_NLABELS; i++)
        emit_string(NVT_NLABELS + i, extra_labels[i]);
    snapshot_modules(NTR_MOD_INITIAL);
    dev_init();
    gm_init();
    maybe_hook_modules(NULL);
    nvt_note(0, "nvtrace %s started (%s), %ld hooks, log %ls", NVTRACE_VERSION, start_reason,
             (long)nhooks, log_path);
    InterlockedExchange(&nvt_log_ready, 1);
    aux_thread = CreateThread(NULL, 0, aux_main, NULL, 0, NULL);
    if (t)
        t->busy = 0;
}

static void start(void)
{
    GetModuleFileNameW(self_module, self_path, MAX_PATH);
    self_path[MAX_PATH - 1] = 0;
    lstrcpynW(self_dir, self_path, MAX_PATH);
    wchar_t *s = wcsrchr(self_dir, L'\\');
    if (s)
        *s = 0;
    QueryPerformanceFrequency(&nvt_qpc_freq);
    nvt_tls = TlsAlloc();
    ts_heap = HeapCreate(0, 0, 0);
    hooks = VirtualAlloc(NULL, sizeof(nvt_hook) * MAX_HOOKS, MEM_RESERVE | MEM_COMMIT,
                         PAGE_READWRITE);
    if (nvt_tls == TLS_OUT_OF_INDEXES || !ts_heap || !hooks)
        return;
    load_config();
    p_capture_stack = (RtlCaptureStackBackTrace_t)(void *)GetProcAddress(
        GetModuleHandleW(L"kernel32.dll"), "RtlCaptureStackBackTrace");
    p_read_vm = (NtReadVirtualMemory_t)(void *)GetProcAddress(GetModuleHandleW(L"ntdll.dll"),
                                                              "NtReadVirtualMemory");
    if (!p_read_vm)
        return;
    /* never unload: patched code jumps into this DLL, and in lazy mode the
     * loader notification does */
    HMODULE pin;
    GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_PIN | GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
                       (LPCWSTR)(void *)start, &pin);
    LdrRegisterDllNotification_t reg = (LdrRegisterDllNotification_t)(void *)GetProcAddress(
        GetModuleHandleW(L"ntdll.dll"), "LdrRegisterDllNotification");
    if (reg)
        reg(0, (PVOID)dll_notify, NULL, &dll_cookie);
    if (!nvt_cfg.lazy) {
        full_start("load");
        return;
    }
    /* lazy: start now only if a trigger module is already there */
    char which[64];
    if (trigger_loaded(which, sizeof(which))) {
        char why[96];
        _snprintf(why, sizeof(why) - 1, "already-loaded:%s", which);
        why[sizeof(why) - 1] = 0;
        full_start(why);
    }
}

/* END record. at_exit: every other thread is gone (DLL_PROCESS_DETACH at
 * process exit): unmap and truncate the log. Otherwise (stop file,
 * TerminateProcess on ourselves) the log is closed for further records and
 * the hooks pass through from now on. */
static void finish(int at_exit)
{
    if (!nvt_log_ready || InterlockedExchange(&nvt_dead, 1))
        return;
    if (at_exit)
        nvt_log_exit_mode();
    else
        dev_pending_poll(1);
    if (at_exit)
        snapshot_modules(NTR_MOD_FINAL);
    uint8_t store[sizeof(ntr_rec_hdr) + sizeof(ntr_end) + 8];
    nvt_buf b = {store, 0, sizeof(store), 0, 1};
    ntr_end e;
    e.records = (uint64_t)nvt_stat_records + 1;
    e.calls = (uint64_t)nvt_stat_calls;
    e.dropped = (uint64_t)nvt_stat_dropped;
    e.bytes = (uint64_t)nvt_stat_bytes;
    nvt_rec_begin(&b, NTR_REC_END);
    nvt_buf_put(&b, &e, sizeof(e));
    nvt_rec_end(&b);
    nvt_log_close(at_exit);
}

/* ------------------------------------------------------------------ selftest */

/* asm: syscall-stub shaped functions returning x + 0x1234/0x2234/0x3234 */
#ifdef _WIN64
#define ST_CC
#else
#define ST_CC __stdcall
#endif
extern int ST_CC nvt_selftest_target(int x);
extern int ST_CC nvt_selftest_target2(int x);
extern int ST_CC nvt_selftest_target3(int x);

static volatile LONG st_handler_calls;

static uintptr_t st_handler(nvt_hook *h, uintptr_t *args, void *ret)
{
    (void)ret;
    InterlockedIncrement(&st_handler_calls);
    return nvt_call_orig(h->orig, args, h->nargs);
}

static volatile LONG st_stop, st_bad, st_runs;
static int st_prepared;

static DWORD WINAPI st_hammer(LPVOID arg)
{
    int base = (int)(intptr_t)arg;
    while (!st_stop) {
        for (int i = 0; i < 1000; i++) {
            if (nvt_selftest_target(base + i) != 0x1234 + base + i)
                InterlockedIncrement(&st_bad);
            if (nvt_selftest_target2(base + i) != 0x2234 + base + i)
                InterlockedIncrement(&st_bad);
            if (nvt_selftest_target3(base + i) != 0x3234 + base + i)
                InterlockedIncrement(&st_bad);
        }
        InterlockedIncrement(&st_runs);
    }
    return 0;
}

static void st_out(char *out, size_t outsz, size_t *pos, const char *fmt, ...)
{
    va_list ap;
    va_start(ap, fmt);
    if (*pos < outsz) {
        int n = _vsnprintf(out + *pos, outsz - *pos - 1, fmt, ap);
        if (n < 0 || (size_t)n >= outsz - *pos - 1)
            *pos = outsz - 1;
        else
            *pos += (size_t)n;
        out[*pos] = 0;
    }
    va_end(ap);
}

typedef LONG(APIENTRY *PFN_ENUMADAPTERS2)(void *);
typedef LONG(APIENTRY *PFN_KMT1)(void *);

/* Exported for nvinject --selftest: checks the hooks of this process and the
 * live-patching path. Writes a report to out; returns the number of failed
 * checks. Never throws, never leaves threads running. */
__declspec(dllexport) int __stdcall nvtrace_selftest(char *out, unsigned outsz);
__declspec(dllexport) int __stdcall nvtrace_selftest(char *out, unsigned outsz)
{
    size_t pos = 0;
    int fails = 0;
    if (!out || outsz < 64)
        return 1;
    out[0] = 0;
#define CHECKT(cond, ...)                                                       \
    do {                                                                        \
        if (cond) {                                                             \
            st_out(out, outsz, &pos, "  ok    ");                                \
        } else {                                                                \
            st_out(out, outsz, &pos, "  FAIL  ");                                \
            fails++;                                                            \
        }                                                                       \
        st_out(out, outsz, &pos, __VA_ARGS__);                                  \
        st_out(out, outsz, &pos, "\n");                                         \
    } while (0)
    CHECKT(nvt_log_ready && !nvt_dead, "log open and ready (%ls)", log_path);
    if (!nvt_log_ready)
        return fails;
    int installed = 0, failed = 0, bad_patch = 0, w32 = 0, nt = 0, dxgk = 0;
    for (LONG i = 0; i < nhooks && i < MAX_HOOKS; i++) {
        nvt_hook *h = &hooks[i];
        if (!h->installed) {
            if (h->target)
                failed++;
            continue;
        }
        installed++;
        if (!strcmp(h->module, "win32u.dll"))
            w32++;
        if (!strcmp(h->module, "ntdll.dll"))
            nt++;
        if (!strncmp(h->name, "NtDxgk", 6))
            dxgk++;
        uint8_t now[8];
        int32_t rel;
        if (nvt_safe_copy(now, h->target, 5) != 5 || now[0] != 0xE9) {
            bad_patch++;
            continue;
        }
        memcpy(&rel, now + 1, 4);
        if ((uintptr_t)h->target + 5 + (intptr_t)rel != (uintptr_t)h->entry)
            bad_patch++;
    }
    CHECKT(installed > 0, "%d hooks installed (win32u %d of which NtDxgk %d, ntdll %d), %d not installed",
           installed, w32, dxgk, nt, failed);
    CHECKT(w32 >= 100, "win32u D3DKMT thunks hooked: %d (expect > 100 on Windows 10/11)", w32);
    CHECKT(bad_patch == 0, "every installed patch is a jmp to its own stub (%d wrong)", bad_patch);
    for (LONG i = 0; i < nhooks && i < MAX_HOOKS; i++)
        if (!hooks[i].installed && hooks[i].target && pos < outsz - 200)
            st_out(out, outsz, &pos, "        not hooked: %s!%s\n", hooks[i].module, hooks[i].name);

    tstate *t = ts_get();
    int on_wine = GetProcAddress(GetModuleHandleW(L"ntdll.dll"), "wine_get_version") != NULL;
    /* the win32u thunks themselves, then the gdi32 D3DKMT* entry points that
     * runtimes and drivers call (on Windows these jump to win32u) */
    HMODULE w32u = GetModuleHandleW(L"win32u.dll");
    /* OpenAdapterFromLuid with a zero LUID: fails harmlessly everywhere */
    PFN_KMT1 nt_open = w32u ? (PFN_KMT1)(void *)GetProcAddress(w32u, "NtGdiDdDDIOpenAdapterFromLuid") : NULL;
    LONG64 c0 = nvt_stat_calls;
    if (nt_open) {
        struct {
            LUID luid;
            UINT hAdapter;
        } q;
        memset(&q, 0, sizeof(q));
        if (nt_open(&q) >= 0 && q.hAdapter) {
            PFN_KMT1 nt_close = (PFN_KMT1)(void *)GetProcAddress(w32u, "NtGdiDdDDICloseAdapter");
            UINT h = q.hAdapter;
            if (nt_close)
                nt_close(&h);
        }
    }
    LONG64 cw = nvt_stat_calls;
    CHECKT(nt_open && cw - c0 >= 1, "win32u!NtGdiDdDDIOpenAdapterFromLuid call logged (%lld calls)",
           (long long)(cw - c0));
    c0 = cw;
    HMODULE gdi = GetModuleHandleW(L"gdi32.dll");
    if (!gdi)
        gdi = LoadLibraryW(L"gdi32.dll");
    PFN_ENUMADAPTERS2 enum2 = gdi ? (PFN_ENUMADAPTERS2)(void *)GetProcAddress(gdi, "D3DKMTEnumAdapters2") : NULL;
    PFN_KMT1 close_adapter = gdi ? (PFN_KMT1)(void *)GetProcAddress(gdi, "D3DKMTCloseAdapter") : NULL;
    PFN_KMT1 qai = gdi ? (PFN_KMT1)(void *)GetProcAddress(gdi, "D3DKMTQueryAdapterInfo") : NULL;
    int nad = 0;
    if (enum2) {
        struct {
            ULONG NumAdapters;
            void *pAdapters;
        } e2 = {0, NULL};
        struct {
            UINT hAdapter;
            LUID luid;
            ULONG nsrc;
            BOOL precise;
        } ad[16];
        enum2(&e2);
        if (e2.NumAdapters > 16)
            e2.NumAdapters = 16;
        e2.pAdapters = ad;
        if (enum2(&e2) >= 0)
            nad = (int)e2.NumAdapters;
        for (int i = 0; i < nad; i++) {
            if (qai) {
                struct {
                    UINT hAdapter;
                    UINT Type;
                    void *p;
                    UINT size;
                } q = {ad[i].hAdapter, 1 /* KMTQAITYPE_UMDRIVERNAME */, NULL, 0};
                struct {
                    UINT type;
                    wchar_t name[MAX_PATH];
                } un;
                memset(&un, 0, sizeof(un));
                un.type = 2;   /* KMTUMDVERSION_DX11 */
                q.p = &un;
                q.size = sizeof(un);
                if (qai(&q) >= 0)
                    st_out(out, outsz, &pos, "        adapter %d: D3D11 UMD %ls\n", i, un.name);
            }
            if (close_adapter) {
                UINT h = ad[i].hAdapter;
                close_adapter(&h);
            }
        }
    }
    LONG64 c1 = nvt_stat_calls;
    if (on_wine && !(enum2 && c1 - c0 >= 2))
        st_out(out, outsz, &pos, "  note  gdi32 D3DKMT calls logged: %lld (Wine's gdi32 bypasses win32u)\n",
               (long long)(c1 - c0));
    else
        CHECKT(enum2 && c1 - c0 >= 2, "gdi32 D3DKMT* calls logged: %lld (%d adapters)", (long long)(c1 - c0),
               nad);
    /* device open by name is logged even if it fails */
    HANDLE dh = CreateFileW(L"\\\\.\\NvAdminDevice", 0, FILE_SHARE_READ | FILE_SHARE_WRITE, NULL,
                            OPEN_EXISTING, 0, NULL);
    LONG64 c2 = nvt_stat_calls;
    CHECKT(c2 > c1, "\\\\.\\NvAdminDevice open logged (%s)", dh != INVALID_HANDLE_VALUE ? "opened" : "failed");
    if (dh != INVALID_HANDLE_VALUE)
        CloseHandle(dh);
    /* registry: the display adapter class key (opened or not, the open is
     * logged by name), and a value read through it */
    LONG64 c3 = nvt_stat_calls;
    HKEY hk = NULL;
    LONG rr = RegOpenKeyExW(HKEY_LOCAL_MACHINE,
                            L"SYSTEM\\CurrentControlSet\\Control\\Class\\{4d36e968-e325-11ce-bfc1-08002be10318}",
                            0, KEY_READ, &hk);
    if (!rr) {
        wchar_t cls[64];
        DWORD cb = sizeof(cls);
        RegQueryValueExW(hk, L"Class", NULL, NULL, (BYTE *)cls, &cb);
        RegCloseKey(hk);
    }
    LONG64 c4 = nvt_stat_calls;
    CHECKT(!nvt_cfg.reg || c4 - c3 >= (rr ? 1 : 2), "display class registry key: %lld calls logged (key %s)",
           (long long)(c4 - c3), rr ? "absent" : "present");
    /* a file path with "nvidia" in it is logged even if it does not exist */
    HANDLE fh2 = CreateFileW(L"C:\\nvtrace-selftest-nvidia.missing", GENERIC_READ, FILE_SHARE_READ, NULL,
                             OPEN_EXISTING, 0, NULL);
    if (fh2 != INVALID_HANDLE_VALUE)
        CloseHandle(fh2);
    LONG64 c5 = nvt_stat_calls;
    CHECKT(nvt_cfg.files == 0 || c5 > c4, "NVIDIA-related file open logged");

    /* live patching under load: four threads call a syscall-stub shaped
     * function in a tight loop while it is hooked */
    int was_busy = t ? t->busy : 0;
    if (t)
        t->busy = 1;
    HANDLE th[4];
    int nth = 0;
    st_stop = 0;
    st_bad = 0;
    st_runs = 0;
    for (int i = 0; i < 4; i++) {
        th[nth] = CreateThread(NULL, 0, st_hammer, (LPVOID)(intptr_t)(i * 100000), 0, NULL);
        if (th[nth])
            nth++;
    }
    Sleep(50);
    batch *bt = batch_alloc();
    LONG before_runs = st_runs;
    if (bt) {
        hook_add(bt, "nvtrace", "nvt_selftest_target", (void *)nvt_selftest_target, NULL, st_handler, 0, 1);
        hook_add(bt, "nvtrace", "nvt_selftest_target2", (void *)nvt_selftest_target2, NULL, st_handler, 0, 1);
        hook_add(bt, "nvtrace", "nvt_selftest_target3", (void *)nvt_selftest_target3, NULL, st_handler, 0, 1);
        st_prepared = bt->n;
        batch_commit(bt);
        VirtualFree(bt, 0, MEM_RELEASE);
    }
    if (t)
        t->busy = was_busy;
    Sleep(200);
    st_stop = 1;
    WaitForMultipleObjects(nth, th, TRUE, 5000);
    for (int i = 0; i < nth; i++)
        CloseHandle(th[i]);
    int st_hooked = 0;
    for (LONG i = 0; i < nhooks && i < MAX_HOOKS; i++)
        if (hooks[i].installed && !strcmp(hooks[i].module, "nvtrace"))
            st_hooked++;
    CHECKT(st_prepared == 3 && st_hooked == 3,
           "3 test functions (8-byte, 16-byte and frozen-thread patch paths) hooked: %d of %d", st_hooked,
           st_prepared);
    CHECKT(nth == 4 && st_runs > before_runs, "4 threads ran %ld x 3000 calls across the live patch",
           (long)st_runs);
    CHECKT(st_bad == 0, "no wrong results while patching (%ld wrong)", (long)st_bad);
    CHECKT(st_handler_calls > 0, "calls went through the new hook: %ld", (long)st_handler_calls);
    int direct = nvt_selftest_target(5) + nvt_selftest_target2(5) + nvt_selftest_target3(5);
    CHECKT(direct == 0x1234 + 0x2234 + 0x3234 + 15, "hooked functions still return the right values");
    st_out(out, outsz, &pos, "  log   %ls\n", log_path);
    return fails;
#undef CHECKT
}

/* ------------------------------------------------------------------ DllMain */

BOOL WINAPI DllMain(HINSTANCE inst, DWORD reason, LPVOID reserved)
{
    switch (reason) {
    case DLL_PROCESS_ATTACH:
        self_module = inst;
        start();
        break;
    case DLL_THREAD_DETACH:
        ts_free();
        break;
    case DLL_PROCESS_DETACH:
        if (reserved)
            finish(1);   /* process exit: the other threads are gone; nvt_dead
                          * is set first, so hooked calls pass through */
        break;
    }
    return TRUE;
}
