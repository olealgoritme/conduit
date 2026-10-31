/* nvtrace: capture every D3DKMT thunk (win32u NtGdiDdDDI*), device I/O on
 * NVIDIA/graphics device handles, section maps and module loads of the
 * process it is loaded into. See README.md for the log format. */
#include "nvtrace.h"
#include <string.h>
#include <stdio.h>
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
LARGE_INTEGER nvt_qpc_freq;
DWORD nvt_tls = TLS_OUT_OF_INDEXES;

static volatile LONG log_ready;
static HMODULE self_module;
static wchar_t self_path[MAX_PATH];
static void *dll_cookie;

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
} tstate;

static HANDLE ts_heap;

static tstate *ts_get(void)
{
    void *v = TlsGetValue(nvt_tls);
    if (v == (void *)1)
        return NULL;                    /* writer thread */
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
    TlsSetValue(nvt_tls, NULL);
    for (int i = 0; i < MAX_DEPTH; i++)
        nvt_buf_free(&t->bufs[i]);
    HeapFree(ts_heap, 0, t);
}

/* ------------------------------------------------------------------ safe reads */

/* Memory named by the traced program is read with NtReadVirtualMemory on our
 * own process: a bad pointer gives a short copy (STATUS_PARTIAL_COPY or an
 * error), never an exception that other vectored handlers could see. */
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

static int safe_read(void *dst, uintptr_t src, size_t n)
{
    if (!src)
        return 0;
    return nvt_safe_copy(dst, (const void *)src, n) == n;
}

/* ------------------------------------------------------------------ handles */

typedef struct tracked {
    HANDLE h;
    char name[64];
} tracked;

#define MAX_TRACKED 256
static tracked tracked_tab[MAX_TRACKED];
static SRWLOCK tracked_lock = SRWLOCK_INIT;
static volatile LONG ntracked;

static int handle_tracked(HANDLE h)
{
    if (!ntracked || !h)
        return 0;
    int found = 0;
    AcquireSRWLockShared(&tracked_lock);
    for (int i = 0; i < MAX_TRACKED; i++)
        if (tracked_tab[i].h == h) {
            found = 1;
            break;
        }
    ReleaseSRWLockShared(&tracked_lock);
    return found;
}

static void handle_track(HANDLE h, const char *name)
{
    AcquireSRWLockExclusive(&tracked_lock);
    for (int i = 0; i < MAX_TRACKED; i++)
        if (!tracked_tab[i].h || tracked_tab[i].h == h) {
            if (!tracked_tab[i].h)
                InterlockedIncrement(&ntracked);
            tracked_tab[i].h = h;
            lstrcpynA(tracked_tab[i].name, name, sizeof(tracked_tab[i].name));
            break;
        }
    ReleaseSRWLockExclusive(&tracked_lock);
}

static void handle_untrack(HANDLE h)
{
    AcquireSRWLockExclusive(&tracked_lock);
    for (int i = 0; i < MAX_TRACKED; i++)
        if (tracked_tab[i].h == h) {
            tracked_tab[i].h = NULL;
            InterlockedDecrement(&ntracked);
        }
    ReleaseSRWLockExclusive(&tracked_lock);
}

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

/* Device-like: \??\Name or \Device\Name, but not \??\C:\... or \??\UNC\... */
static int device_like(const char *n)
{
    if (!strncmp(n, "\\device\\", 8))
        return 1;
    if (!strncmp(n, "\\??\\", 4) || !strncmp(n, "\\\\?\\", 4) || !strncmp(n, "\\\\.\\", 4)) {
        const char *r = n + 4;
        if (r[0] && r[1] == ':')
            return 0;
        if (!strncmp(r, "unc\\", 4))
            return 0;
        return r[0] != 0;
    }
    return 0;
}

static int device_interesting(const char *n)
{
    if (nvt_cfg.devices_all)
        return 1;
    const char *r = n;
    if (!strncmp(r, "\\device\\", 8))
        r += 8;
    else
        r += 4;
    return strstr(r, "nv") || strstr(r, "uvm") || strstr(r, "dxg") || strstr(r, "gpu") ||
           strstr(r, "nvidia");
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

typedef struct capture {
    nvt_buf *b;
    uint32_t nblobs;            /* blobs in the record so far */
    uint32_t flags;             /* NTR_CALL_* */
    size_t first_blob;          /* offset of the first blob */
    uint32_t before_blobs;      /* blobs written in phase 0 */
    uint64_t chase_budget;      /* bytes left for chased copies */
} capture;

static uint64_t rd_le(const uint8_t *p, int w)
{
    uint64_t v = 0;
    memcpy(&v, p, w);
    return v;
}

static size_t find_before(capture *c, uint32_t label, uint32_t index, uint64_t addr,
                          uint32_t *cap_out)
{
    size_t off = c->first_blob;
    for (uint32_t i = 0; i < c->before_blobs && off < c->b->len; i++) {
        ntr_blob *bl = (ntr_blob *)(c->b->p + off);
        if (bl->label == label && bl->index == index && bl->addr == addr && bl->phase == 0 &&
            !(bl->flags & NTR_BLOB_SAME)) {
            *cap_out = bl->cap_size;
            return off + sizeof(ntr_blob);
        }
        off += sizeof(ntr_blob) + ((bl->cap_size + 7) & ~7u);
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

static int user_ptr_like(uint64_t v)
{
#ifdef _WIN64
    return v >= 0x10000 && v < 0x00007FFFFFFF0000ull && (v & 3) == 0;
#else
    return v >= 0x10000 && v < 0xFFFF0000ull && (v & 3) == 0;
#endif
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
        if ((uint32_t)pd->szoff + pd->szw > plen)
            return;
        req = rd_le(c->b->p + poff + pd->szoff, pd->szw) * (uint64_t)pd->fixed;
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
    if (!strcmp(h->name, "NtGdiDdDDICreateContext")) {
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
    if (start == 0xFFFFFFFFu) {
        return 1;
    }
    for (uint32_t i = start; i < got && n < max; i++)
        out[n++] = (uintptr_t)fr[i];
    return n ? n : 1;
}

/* ------------------------------------------------------------------ dispatch */

static uintptr_t h_createprocess(nvt_hook *h, uintptr_t *args, void *ret);
static void finish(int final_snapshot);

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
    if (nvt_dead || !log_ready)
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
            /* ExitProcess: other threads (the writer too) die next */
            nvt_log_sync(2000);
        } else if (ph == (HANDLE)(intptr_t)-1 || GetProcessId(ph) == GetCurrentProcessId()) {
            t->busy = 1;
            nvt_note(0, "NtTerminateProcess(self, 0x%lx)", (unsigned long)args[1]);
            finish(0);
            t->busy = 0;
        }
        return passthrough(h, args, le);
    }
    if ((h->flags & NVT_FN_DEVHANDLE) && !handle_tracked((HANDLE)args[0]))
        return passthrough(h, args, le);
    if ((h->flags & NVT_FN_CLOSE) && !handle_tracked((HANDLE)args[0]))
        return passthrough(h, args, le);
    if ((h->flags & NVT_FN_MAP) && !nvt_cfg.maps)
        return passthrough(h, args, le);

    t->busy = 1;
    nvt_buf *b = &t->bufs[t->depth];
    capture c;
    memset(&c, 0, sizeof(c));
    c.b = b;
    c.chase_budget = (uint64_t)nvt_cfg.chase * nvt_cfg.chase_max * 4;

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
    /* args as stored (u64 each); the walker reads them through a P-sized
     * slot array kept right after, so copy the native slots too */
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
        int dev = !rel && device_like(name);
        if (dev && device_interesting(name)) {
            if (ok)
                handle_track(fh, name);     /* failed opens are logged too */
        } else if (!nvt_cfg.files) {
            keep = 0;
        }
    }
    if (keep) {
        capture_phase(&c, h, slots_off, 1, args);
        if ((h->flags & NVT_FN_CTXTRACK) && ok)
            ctx_after(h, args);
        if ((h->flags & NVT_FN_RENDER) && ok)
            render_after(args);
        if ((h->flags & NVT_FN_CLOSE) && ok)
            handle_untrack((HANDLE)args[0]);
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
            /* drop the native slot copy */
            memmove(b->p + slots_off, b->p + slots_end, b->len - slots_end);
            b->len -= slots_end - slots_off;
        }
        nvt_rec_end(b);
        InterlockedIncrement64(&nvt_stat_calls);
    }
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

static int inject_same_arch(HANDLE proc)
{
    size_t bytes = (wcslen(self_path) + 1) * sizeof(wchar_t);
    void *rp = VirtualAllocEx(proc, NULL, bytes, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    if (!rp)
        return -1;
    if (!WriteProcessMemory(proc, rp, self_path, bytes, NULL))
        return -1;
    FARPROC ll = GetProcAddress(GetModuleHandleW(L"kernel32.dll"), "LoadLibraryW");
    HANDLE th = CreateRemoteThread(proc, NULL, 0, (LPTHREAD_START_ROUTINE)(void *)ll, rp, 0, NULL);
    if (!th)
        return -1;
    DWORD w = WaitForSingleObject(th, 15000);
    DWORD code = 0;
    GetExitCodeThread(th, &code);
    CloseHandle(th);
    return (w == WAIT_OBJECT_0 && code) ? 0 : -1;
}

static int inject_other_arch(DWORD pid)
{
    wchar_t dir[MAX_PATH], cmd[MAX_PATH * 2];
    lstrcpynW(dir, self_path, MAX_PATH);
    wchar_t *s = wcsrchr(dir, L'\\');
    if (!s)
        return -1;
    *s = 0;
#ifdef _WIN64
    const wchar_t *helper = L"nvinject32.exe";
#else
    const wchar_t *helper = L"nvinject.exe";
#endif
    _snwprintf(cmd, MAX_PATH * 2 - 1, L"\"%ls\\%ls\" -p %lu", dir, helper, (unsigned long)pid);
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
        int rc = same ? inject_same_arch(pi.hProcess) : inject_other_arch(pi.dwProcessId);
        nvt_note(rc ? 1 : 0, "child pid %lu: %s (%s)", (unsigned long)pi.dwProcessId,
                 rc ? "injection failed" : "injected", same ? "same arch" : "other arch");
        if (!was_suspended)
            ResumeThread(pi.hThread);
    }
    t->busy = 0;
    SetLastError(le);
    return r;
}

/* ------------------------------------------------------------------ install */

static uint32_t string_ids;

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

static int excluded(const char *name)
{
    const char *e = nvt_cfg.exclude;
    size_t n = strlen(name);
    while (*e) {
        const char *comma = strchr(e, ',');
        size_t len = comma ? (size_t)(comma - e) : strlen(e);
        if (len == n && !_strnicmp(e, name, n))
            return 1;
        if (!comma)
            break;
        e = comma + 1;
    }
    return 0;
}

static const nvt_fdesc *find_fdesc(const char *name)
{
    for (size_t i = 0; i < NFDESCS; i++)
        if (!strcmp(nvt_fdescs[i].name, name))
            return &nvt_fdescs[i];
    return NULL;
}

static void hook_one(HMODULE mod, const char *modname, const char *name, void *addr,
                     const nvt_fdesc *fd, nvt_handler handler, uint32_t extra_flags,
                     uint32_t nargs_hint)
{
    (void)mod;
    LONG idx = InterlockedIncrement(&nhooks) - 1;
    if (idx >= MAX_HOOKS)
        return;
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
    h->syscall_nr = addr ? nvt_stub_syscall_nr(addr) : 0xFFFFFFFFu;
    if (h->syscall_nr != 0xFFFFFFFFu)
        h->hookflags |= NTR_HOOK_SYSCALL_STUB;
    h->nargs = fd ? fd->nargs : nargs_hint;
    if (!addr) {
        emit_hook(h, NTR_HOOKERR_NOTFOUND);
        return;
    }
    if (excluded(name)) {
        emit_hook(h, NTR_HOOKERR_EXCLUDED);
        return;
    }
#ifndef _WIN64
    uint32_t ab;
    if (h->syscall_nr != 0xFFFFFFFFu && !nvt_stub_argbytes(addr, &ab)) {
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
    nvt_hooks_begin();
    int rc = nvt_hook_install(h, &err);
    nvt_hooks_end();
    emit_hook(h, rc ? (err ? err : NTR_HOOKERR_DECODE) : 0);
}

static void hook_module_table(HMODULE mod, const char *modname)
{
    for (size_t i = 0; i < NFDESCS; i++) {
        if (_stricmp(nvt_fdescs[i].module, modname))
            continue;
        if (!strncmp(nvt_fdescs[i].name, "NtGdiDdDDI", 10))
            continue;                   /* done by the export scan */
        void *a = (void *)GetProcAddress(mod, nvt_fdescs[i].name);
        if (!a)
            continue;                   /* e.g. NtUnmapViewOfSectionEx on old builds */
        hook_one(mod, modname, nvt_fdescs[i].name, a, &nvt_fdescs[i], NULL, 0, 0);
    }
}

/* every NtGdiDdDDI* export of win32u, known or not */
static void hook_win32u(HMODULE mod)
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
    for (DWORD i = 0; i < ex->NumberOfNames; i++) {
        const char *name = (const char *)(base + names[i]);
        if (strncmp(name, "NtGdiDdDDI", 10))
            continue;
        DWORD rva = funcs[ords[i]];
        if (rva >= dd->VirtualAddress && rva < dd->VirtualAddress + dd->Size)
            continue;                   /* forwarder */
        const nvt_fdesc *fd = find_fdesc(name);
        hook_one(mod, "win32u.dll", name, base + rva, fd, NULL, 0, fd ? 0 : (P == 8 ? 8 : 0));
    }
}

/* Hooks each module once. win32u may arrive late (from the DLL load
 * notification, which passes its base: the loader cannot look it up yet). */
static void maybe_hook_modules(HMODULE win32u_base)
{
    HMODULE m;
    if (nvt_cfg.ntdll && (m = GetModuleHandleW(L"ntdll.dll")) &&
        !InterlockedCompareExchange(&ntdll_hooked, 1, 0))
        hook_module_table(m, "ntdll.dll");
    m = win32u_base ? win32u_base : GetModuleHandleW(L"win32u.dll");
    if (m && !InterlockedCompareExchange(&win32u_hooked, 1, 0))
        hook_win32u(m);
    if (nvt_cfg.follow && (m = GetModuleHandleW(L"kernelbase.dll")) &&
        !InterlockedCompareExchange(&kernelbase_hooked, 1, 0)) {
        void *a = (void *)GetProcAddress(m, "CreateProcessInternalW");
        hook_one(m, "kernelbase.dll", "CreateProcessInternalW", a, NULL, h_createprocess,
                 NVT_FN_PROCESS, 12);
    }
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

static void emit_module(uintptr_t base, uint64_t size, const wchar_t *path, size_t pathlen,
                        uint32_t flags)
{
    nvt_buf b = {0};
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
    nvt_buf_free(&b);
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

static void snapshot_modules(uint32_t flags)
{
    uint8_t *peb;
#ifdef _WIN64
    __asm__ volatile("movq %%gs:0x60, %0" : "=r"(peb));
    nvt_peb_ldr *ldr = *(nvt_peb_ldr **)(peb + 0x18);
#else
    __asm__ volatile("movl %%fs:0x30, %0" : "=r"(peb));
    nvt_peb_ldr *ldr = *(nvt_peb_ldr **)(peb + 0x0C);
#endif
    if (!ldr)
        return;
    LIST_ENTRY *head = &ldr->InLoadOrderModuleList;
    int guard = 0;
    for (LIST_ENTRY *e = head->Flink; e != head && guard < 4096; e = e->Flink, guard++) {
        nvt_ldr_entry *m = CONTAINING_RECORD(e, nvt_ldr_entry, InLoadOrderLinks);
        emit_module((uintptr_t)m->DllBase, m->SizeOfImage, m->FullDllName.Buffer,
                    m->FullDllName.Length / 2, flags);
    }
}

typedef struct nvt_dll_notif {
    ULONG Flags;
    const nvt_unicode *FullDllName;
    const nvt_unicode *BaseDllName;
    PVOID DllBase;
    ULONG SizeOfImage;
} nvt_dll_notif;

static VOID CALLBACK dll_notify(ULONG reason, const nvt_dll_notif *d, PVOID ctx)
{
    (void)ctx;
    if (nvt_dead || !log_ready)
        return;
    DWORD le = GetLastError();
    tstate *t = ts_get();
    int was_busy = t ? t->busy : 0;
    if (t)
        t->busy = 1;
    if (reason == 1) {
        emit_module((uintptr_t)d->DllBase, d->SizeOfImage, d->FullDllName->Buffer,
                    d->FullDllName->Length / 2, NTR_MOD_LOADED);
        if (d->BaseDllName && d->BaseDllName->Buffer) {
            wchar_t nm[32];
            size_t n = d->BaseDllName->Length / 2;
            if (n < 32) {
                memcpy(nm, d->BaseDllName->Buffer, n * 2);
                nm[n] = 0;
                if (!_wcsicmp(nm, L"win32u.dll"))
                    maybe_hook_modules((HMODULE)d->DllBase);
            }
        }
    } else if (reason == 2) {
        nvt_buf b = {0};
        char u8[MAX_PATH * 3];
        uint64_t base = (uintptr_t)d->DllBase;
        int n = WideCharToMultiByte(CP_UTF8, 0, d->FullDllName->Buffer,
                                    d->FullDllName->Length / 2, u8, sizeof(u8) - 1, NULL, NULL);
        if (n < 0)
            n = 0;
        u8[n] = 0;
        nvt_rec_begin(&b, NTR_REC_MODULE_UNLOAD);
        nvt_buf_put(&b, &base, 8);
        nvt_buf_put(&b, u8, (size_t)n + 1);
        nvt_rec_end(&b);
        nvt_buf_free(&b);
    }
    if (t)
        t->busy = was_busy;
    SetLastError(le);
}

/* ------------------------------------------------------------------ config */

static uint64_t env_u64(const wchar_t *name, uint64_t def)
{
    wchar_t v[64];
    DWORD n = GetEnvironmentVariableW(name, v, 64);
    if (!n || n >= 64)
        return def;
    return _wcstoui64(v, NULL, 0);
}

static void load_config(void)
{
    wchar_t v[MAX_PATH];
    DWORD n = GetEnvironmentVariableW(L"NVTRACE_DIR", nvt_cfg.dir, MAX_PATH);
    if (!n || n >= MAX_PATH)
        lstrcpyW(nvt_cfg.dir, L"C:\\nvtrace");
    nvt_cfg.rawsize = (uint32_t)env_u64(L"NVTRACE_RAWSIZE", 512);
    nvt_cfg.maxbuf = env_u64(L"NVTRACE_MAXBUF", 64ull << 20);
    if (nvt_cfg.maxbuf > (sizeof(void *) == 8 ? (1ull << 30) : (256ull << 20)))
        nvt_cfg.maxbuf = sizeof(void *) == 8 ? (1ull << 30) : (256ull << 20);
    nvt_cfg.stack = (uint32_t)env_u64(L"NVTRACE_STACK", 8);
    if (nvt_cfg.stack > 48)
        nvt_cfg.stack = 48;
    nvt_cfg.files = (int)env_u64(L"NVTRACE_FILES", 0);
    nvt_cfg.maps = (int)env_u64(L"NVTRACE_MAPS", 1);
    nvt_cfg.follow = (int)env_u64(L"NVTRACE_FOLLOW", 0);
    nvt_cfg.chase = (uint32_t)env_u64(L"NVTRACE_CHASE", 4096);
    nvt_cfg.chase_depth = (uint32_t)env_u64(L"NVTRACE_CHASE_DEPTH", 2);
    if (nvt_cfg.chase_depth > 4)
        nvt_cfg.chase_depth = 4;
    nvt_cfg.chase_max = (uint32_t)env_u64(L"NVTRACE_CHASE_MAX", 16);
    if (nvt_cfg.chase_max > 64)
        nvt_cfg.chase_max = 64;
    if (nvt_cfg.chase > (1u << 20))
        nvt_cfg.chase = 1u << 20;
    nvt_cfg.cmdbuf = (uint32_t)env_u64(L"NVTRACE_CMDBUF", 4u << 20);
    nvt_cfg.ntdll = (int)env_u64(L"NVTRACE_NTDLL", 1);
    n = GetEnvironmentVariableW(L"NVTRACE_DEVICES", v, MAX_PATH);
    nvt_cfg.devices_all = n && n < MAX_PATH && !_wcsicmp(v, L"all");
    n = GetEnvironmentVariableW(L"NVTRACE_EXCLUDE", v, MAX_PATH);
    if (n && n < MAX_PATH)
        WideCharToMultiByte(CP_UTF8, 0, v, -1, nvt_cfg.exclude, sizeof(nvt_cfg.exclude) - 1,
                            NULL, NULL);
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

static int open_log(void)
{
    wchar_t exe[MAX_PATH], path[MAX_PATH * 2];
    GetModuleFileNameW(NULL, exe, MAX_PATH);
    wchar_t *base = wcsrchr(exe, L'\\');
    base = base ? base + 1 : exe;
    wchar_t *dot = wcsrchr(base, L'.');
    if (dot)
        *dot = 0;
    mkdirs(nvt_cfg.dir);
    for (int i = 0; i < 100; i++) {
        if (i)
            _snwprintf(path, MAX_PATH * 2 - 1, L"%ls\\%ls-%lu-%d.ntr", nvt_cfg.dir, base,
                       (unsigned long)GetCurrentProcessId(), i);
        else
            _snwprintf(path, MAX_PATH * 2 - 1, L"%ls\\%ls-%lu.ntr", nvt_cfg.dir, base,
                       (unsigned long)GetCurrentProcessId());
        path[MAX_PATH * 2 - 1] = 0;
        if (!nvt_log_open(path, P))
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
                      L"UBR", RRF_RT_REG_DWORD, NULL, &ubr, &sz)) {
        snprintf(t, sizeof(t), "%lu", ubr);
        kv(&b, "os_ubr", t);
    }
    snprintf(t, sizeof(t), "%u", (unsigned)P);
    kv(&b, "ptr_size", t);
    snprintf(t, sizeof(t), "%lu", GetCurrentProcessId());
    kv(&b, "pid", t);
    if (qip) {
        ULONG_PTR pbi[6];
        if (!qip(GetCurrentProcess(), 0, pbi, sizeof(pbi), NULL)) {
            snprintf(t, sizeof(t), "%lu", (unsigned long)pbi[5]);
            kv(&b, "ppid", t);
        }
    }
    BOOL wow = FALSE;
    IsWow64Process(GetCurrentProcess(), &wow);
    kv(&b, "wow64", wow ? "1" : "0");
    GetModuleFileNameW(NULL, w, MAX_PATH);
    kvw(&b, "exe", w);
    kvw(&b, "cmdline", GetCommandLineW());
    GetCurrentDirectoryW(MAX_PATH, w);
    kvw(&b, "cwd", w);
    kv(&b, "nvtrace", NVTRACE_VERSION);
    kvw(&b, "dll", self_path);
    snprintf(t, sizeof(t), "%lld", (long long)nvt_qpc_freq.QuadPart);
    kv(&b, "qpc_freq", t);
    FILETIME ft;
    GetSystemTimeAsFileTime(&ft);
    snprintf(t, sizeof(t), "%llu", ((unsigned long long)ft.dwHighDateTime << 32) | ft.dwLowDateTime);
    kv(&b, "start_filetime", t);
    LARGE_INTEGER q;
    QueryPerformanceCounter(&q);
    snprintf(t, sizeof(t), "%lld", (long long)q.QuadPart);
    kv(&b, "start_qpc", t);
    snprintf(t, sizeof(t), "%u", nvt_cfg.rawsize);
    kv(&b, "config.rawsize", t);
    snprintf(t, sizeof(t), "%llu", (unsigned long long)nvt_cfg.maxbuf);
    kv(&b, "config.maxbuf", t);
    snprintf(t, sizeof(t), "%u", nvt_cfg.stack);
    kv(&b, "config.stack", t);
    kv(&b, "config.devices", nvt_cfg.devices_all ? "all" : "nv");
    kv(&b, "config.files", nvt_cfg.files ? "1" : "0");
    kv(&b, "config.maps", nvt_cfg.maps ? "1" : "0");
    kv(&b, "config.follow", nvt_cfg.follow ? "1" : "0");
    snprintf(t, sizeof(t), "%u/%u/%u", nvt_cfg.chase, nvt_cfg.chase_depth, nvt_cfg.chase_max);
    kv(&b, "config.chase", t);
    snprintf(t, sizeof(t), "%u", nvt_cfg.cmdbuf);
    kv(&b, "config.cmdbuf", t);
    kv(&b, "config.exclude", nvt_cfg.exclude);
    nvt_buf_put(&b, "", 1);
    nvt_rec_end(&b);
    nvt_buf_free(&b);
}

typedef LONG(NTAPI *LdrRegisterDllNotification_t)(ULONG, PVOID, PVOID, PVOID *);
typedef LONG(NTAPI *LdrUnregisterDllNotification_t)(PVOID);

static void start(void)
{
    GetModuleFileNameW(self_module, self_path, MAX_PATH);
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
    if (!p_read_vm || open_log())
        return;
    /* never unload: the patched code jumps into this DLL */
    HMODULE pin;
    GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_PIN | GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
                       (LPCWSTR)(void *)start, &pin);
    emit_procinfo();
    for (uint32_t i = 0; i < NVT_NLABELS; i++)
        emit_string(i, nvt_labels[i]);
    for (uint32_t i = 0; i < LBL_COUNT - NVT_NLABELS; i++)
        emit_string(NVT_NLABELS + i, extra_labels[i]);
    string_ids = LBL_COUNT;
    snapshot_modules(NTR_MOD_INITIAL);
    LdrRegisterDllNotification_t reg = (LdrRegisterDllNotification_t)(void *)GetProcAddress(
        GetModuleHandleW(L"ntdll.dll"), "LdrRegisterDllNotification");
    if (reg)
        reg(0, (PVOID)dll_notify, NULL, &dll_cookie);
    maybe_hook_modules(NULL);
    nvt_note(0, "nvtrace %s started, %ld hooks", NVTRACE_VERSION, (long)nhooks);
    InterlockedExchange(&log_ready, 1);
}

/* END record and final flush; after this the hooks pass through. */
static void finish(int final_snapshot)
{
    if (!log_ready || InterlockedExchange(&nvt_dead, 1))
        return;
    /* stop the writer and write the queue; later records go out directly */
    nvt_log_flush_final();
    if (final_snapshot)
        snapshot_modules(NTR_MOD_FINAL);
    nvt_buf b = {0};
    ntr_end e;
    e.records = (uint64_t)nvt_stat_records + 1;
    e.calls = (uint64_t)nvt_stat_calls;
    e.dropped = (uint64_t)nvt_stat_dropped;
    e.bytes = (uint64_t)nvt_stat_bytes;
    nvt_rec_begin(&b, NTR_REC_END);
    nvt_buf_put(&b, &e, sizeof(e));
    nvt_rec_end(&b);
    nvt_buf_free(&b);
}

static void stop(void)
{
    tstate *t = ts_get();
    if (t)
        t->busy = 1;
    finish(1);
}

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
            stop();             /* process exit; on FreeLibrary the hooks must stay */
        break;
    }
    return TRUE;
}
