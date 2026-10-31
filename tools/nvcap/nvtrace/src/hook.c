/* Inline hooks: prologue relocation into a trampoline, and the patch at the
 * target.
 *
 * Target patch: jmp rel32 (5 bytes) to the entry stub. On x64 the stub lives
 * within +-2 GiB of the target so RIP-relative operands can be re-encoded.
 * Entry stub: mov rax/eax, <nvt_hook*>; jmp nvt_entry_common.
 * Trampoline: the relocated whole instructions covering the patch, then a jump
 * back to the first untouched instruction.
 *
 * Installation is two steps so it is safe in a process whose threads are
 * running (nvinject -p, DWM, a late-loaded win32u):
 *   nvt_hook_prepare  builds stub and trampoline, does not touch the target;
 *   nvt_hooks_commit  suspends every other thread of the process, writes all
 *                     patches (one atomic store where the alignment allows),
 *                     moves any thread whose IP is inside a patched range to
 *                     the matching place in its trampoline, resumes.
 * Nothing between suspend and resume takes a user-mode lock (no heap, no
 * logging), so a suspended thread holding the heap or loader lock cannot
 * deadlock us.
 */
#include "nvtrace.h"
#include <string.h>
#include <winternl.h>
#include "x86dec.h"

#ifdef _WIN64
#define IS64 1
#else
#define IS64 0
#endif
#define PATCH_LEN 5

/* ------------------------------------------------------------------ stubs */

uint32_t nvt_stub_syscall_nr(const uint8_t *p)
{
    return x86_stub_syscall_nr(p, IS64);
}

int nvt_stub_argbytes(const uint8_t *p, uint32_t *argbytes)
{
    return x86_stub_argbytes(p, IS64, argbytes);
}

int nvt_insn_len(const uint8_t *p, int is64)
{
    x86_insn d;
    if (x86_decode(p, is64, &d))
        return -1;
    return d.len;
}

/* ------------------------------------------------------------------ memory */

/* Executable memory near `nearaddr` (x64: within +-2 GiB). Bump allocator
 * over 64 KiB blocks; blocks are never freed (hooks are never removed). */
typedef struct region {
    uint8_t *base;
    size_t used;
    size_t size;
} region;

#define MAX_REGIONS 32
static region regions[MAX_REGIONS];
static int nregions;
static SRWLOCK exec_lock = SRWLOCK_INIT;

static int in_range(uintptr_t a, uintptr_t b)
{
#ifdef _WIN64
    intptr_t d = (intptr_t)(a - b);
    return d > -(intptr_t)0x7FF00000 && d < (intptr_t)0x7FF00000;
#else
    (void)a;
    (void)b;
    return 1;
#endif
}

static uint8_t *alloc_block_near(uintptr_t nearaddr)
{
#ifdef _WIN64
    SYSTEM_INFO si;
    GetSystemInfo(&si);
    uintptr_t gran = si.dwAllocationGranularity;
    uintptr_t lo = nearaddr > 0x70000000u ? nearaddr - 0x70000000u
                                          : (uintptr_t)si.lpMinimumApplicationAddress;
    uintptr_t hi = nearaddr + 0x70000000u;
    /* search downwards from the module first, then upwards */
    for (uintptr_t a = (nearaddr & ~(gran - 1)) - gran; a > lo; a -= gran) {
        MEMORY_BASIC_INFORMATION mbi;
        if (!VirtualQuery((void *)a, &mbi, sizeof(mbi)))
            break;
        if (mbi.State == MEM_FREE) {
            void *p = VirtualAlloc((void *)a, 65536, MEM_RESERVE | MEM_COMMIT, PAGE_EXECUTE_READWRITE);
            if (p)
                return p;
        } else if ((uintptr_t)mbi.AllocationBase < a && mbi.AllocationBase) {
            a = ((uintptr_t)mbi.AllocationBase & ~(gran - 1)) + gran;
        }
    }
    for (uintptr_t a = (nearaddr + gran) & ~(gran - 1); a < hi; a += gran) {
        MEMORY_BASIC_INFORMATION mbi;
        if (!VirtualQuery((void *)a, &mbi, sizeof(mbi)))
            break;
        if (mbi.State == MEM_FREE) {
            void *p = VirtualAlloc((void *)a, 65536, MEM_RESERVE | MEM_COMMIT, PAGE_EXECUTE_READWRITE);
            if (p)
                return p;
        } else {
            uintptr_t next = (uintptr_t)mbi.BaseAddress + mbi.RegionSize;
            if (next > a + gran)
                a = (next & ~(gran - 1)) - gran;
        }
    }
    return NULL;
#else
    (void)nearaddr;
    return VirtualAlloc(NULL, 65536, MEM_RESERVE | MEM_COMMIT, PAGE_EXECUTE_READWRITE);
#endif
}

static uint8_t *exec_alloc(uintptr_t nearaddr, size_t n)
{
    uint8_t *r = NULL;
    n = (n + 15) & ~(size_t)15;
    AcquireSRWLockExclusive(&exec_lock);
    for (int i = 0; i < nregions && !r; i++) {
        region *g = &regions[i];
        if (g->size - g->used >= n && in_range((uintptr_t)g->base, nearaddr) &&
            in_range((uintptr_t)g->base + g->size, nearaddr)) {
            r = g->base + g->used;
            g->used += n;
        }
    }
    if (!r && nregions < MAX_REGIONS) {
        uint8_t *b = alloc_block_near(nearaddr);
        if (b) {
            memset(b, 0xCC, 65536);
            regions[nregions].base = b;
            regions[nregions].size = 65536;
            regions[nregions].used = n;
            nregions++;
            r = b;
        }
    }
    ReleaseSRWLockExclusive(&exec_lock);
    return r;
}

/* ------------------------------------------------------------------ prepare */

static uint8_t *emit_jmp(uint8_t *o, uintptr_t to)
{
#ifdef _WIN64
    return x86_emit_abs_jmp(o, to, 1);
#else
    int32_t rel = (int32_t)(to - ((uintptr_t)o + 5));
    o[0] = 0xE9;
    memcpy(o + 1, &rel, 4);
    return o + 5;
#endif
}

#define STUB_MEM 160
#define TRAMP_OFF 32

int nvt_hook_prepare(nvt_hook *h, uint32_t *err)
{
    uint8_t *t = (uint8_t *)h->target;
    uint8_t *mem, *o;
    int consumed, tlen;

    *err = 0;
    /* a copy of the prologue: the target may change under us (another
     * thread, another hooking tool); commit compares against this copy */
    if (nvt_safe_copy(h->orig_bytes, t, sizeof(h->orig_bytes)) != sizeof(h->orig_bytes)) {
        *err = NTR_HOOKERR_DECODE;
        return -1;
    }
    const uint8_t *ob = h->orig_bytes;
    /* a target that already starts with a jump was patched by someone else
     * (or by us): relocating it could jump back into a foreign patch */
    if (ob[0] == 0xE9 || ob[0] == 0xEB || (ob[0] == 0xFF && ob[1] == 0x25) || ob[0] == 0xCC) {
        *err = NTR_HOOKERR_DECODE;
        return -1;
    }
    mem = exec_alloc((uintptr_t)t, STUB_MEM);
    if (!mem) {
        *err = NTR_HOOKERR_ALLOC;
        return -1;
    }
    intptr_t rel = (intptr_t)mem - ((intptr_t)t + 5);
    if (rel > INT32_MAX || rel < INT32_MIN) {
        *err = NTR_HOOKERR_ALLOC;
        return -1;
    }
    /* entry stub: mov rax/eax, <nvt_hook*>; jmp nvt_entry_common */
    o = mem;
    uintptr_t hp = (uintptr_t)h;
#ifdef _WIN64
    o[0] = 0x48;
    o[1] = 0xB8;
    memcpy(o + 2, &hp, 8);
    emit_jmp(o + 10, (uintptr_t)nvt_entry_common);
#else
    o[0] = 0xB8;
    memcpy(o + 1, &hp, 4);
    emit_jmp(o + 5, (uintptr_t)nvt_entry_common);
#endif
    /* relocate from the private copy, as if it ran at t */
    uint8_t *tramp = mem + TRAMP_OFF;
    consumed = x86_relocate_map(ob, (uintptr_t)t, PATCH_LEN, tramp, (uintptr_t)tramp, IS64, &tlen,
                                &h->relmap);
    if (consumed < 0 || consumed > (int)sizeof(h->orig_bytes) - 4 ||
        TRAMP_OFF + tlen + 14 > STUB_MEM) {
        *err = NTR_HOOKERR_DECODE;
        return -1;
    }
    emit_jmp(tramp + tlen, (uintptr_t)t + consumed);
    FlushInstructionCache(GetCurrentProcess(), mem, STUB_MEM);
    h->entry = mem;
    h->tramp = tramp;
    h->consumed = (uint32_t)consumed;
    h->orig = tramp;
    return 0;
}

/* ------------------------------------------------------------------ threads */

typedef LONG(NTAPI *NtQSI_t)(ULONG, PVOID, ULONG, PULONG);

/* SYSTEM_THREAD_INFORMATION (stable since NT 4) */
typedef struct nvt_sti {
    LARGE_INTEGER KernelTime, UserTime, CreateTime;
    ULONG WaitTime;
    PVOID StartAddress;
    struct {
        HANDLE UniqueProcess, UniqueThread;
    } ClientId;
    LONG Priority, BasePriority;
    ULONG ContextSwitches, ThreadState, WaitReason;
} nvt_sti;

/* Process list (SystemProcessInformation) into a VirtualAlloc'd buffer; no
 * heap use, so it may run while other threads are suspended. Caller frees
 * with VirtualFree(p, 0, MEM_RELEASE). */
void *nvt_process_list(void)
{
    static NtQSI_t qsi;
    if (!qsi)
        qsi = (NtQSI_t)(void *)GetProcAddress(GetModuleHandleW(L"ntdll.dll"),
                                               "NtQuerySystemInformation");
    if (!qsi)
        return NULL;
    ULONG size = 1 << 20;
    for (int tries = 0; tries < 6; tries++) {
        void *p = VirtualAlloc(NULL, size, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE);
        if (!p)
            return NULL;
        ULONG need = 0;
        LONG st = qsi(5 /* SystemProcessInformation */, p, size, &need);
        if (st >= 0)
            return p;
        VirtualFree(p, 0, MEM_RELEASE);
        if (st != (LONG)0xC0000004 /* INFO_LENGTH_MISMATCH */)
            return NULL;
        size = (need > size ? need : size) * 2;
    }
    return NULL;
}

/* thread ids of process `pid` from a process list; returns count */
static int list_threads(void *plist, DWORD pid, DWORD *tids, int max)
{
    uint8_t *p = plist;
    int n = 0;
    for (;;) {
        SYSTEM_PROCESS_INFORMATION *pi = (SYSTEM_PROCESS_INFORMATION *)p;
        if ((DWORD)(uintptr_t)pi->UniqueProcessId == pid) {
            nvt_sti *th = (nvt_sti *)(p + sizeof(SYSTEM_PROCESS_INFORMATION));
            for (ULONG i = 0; i < pi->NumberOfThreads && n < max; i++)
                tids[n++] = (DWORD)(uintptr_t)th[i].ClientId.UniqueThread;
            return n;
        }
        if (!pi->NextEntryOffset)
            return n;
        p += pi->NextEntryOffset;
    }
}

#define MAX_FROZEN 4096

typedef struct frozen {
    int n;
    DWORD tid[MAX_FROZEN];
    HANDLE h[MAX_FROZEN];
    CONTEXT ctx[MAX_FROZEN];
    uint8_t ctx_ok[MAX_FROZEN];
} frozen;

/* Suspend every other thread of this process. Repeats the enumeration until
 * no new thread shows up, so threads created meanwhile are caught too. */
static void freeze_others(frozen *f)
{
    DWORD self = GetCurrentThreadId(), pid = GetCurrentProcessId();
    DWORD *tids = VirtualAlloc(NULL, MAX_FROZEN * sizeof(DWORD), MEM_RESERVE | MEM_COMMIT,
                               PAGE_READWRITE);
    f->n = 0;
    if (!tids)
        return;
    for (int round = 0; round < 8; round++) {
        void *pl = nvt_process_list();
        if (!pl)
            break;
        int nt = list_threads(pl, pid, tids, MAX_FROZEN);
        VirtualFree(pl, 0, MEM_RELEASE);
        int added = 0;
        for (int i = 0; i < nt && f->n < MAX_FROZEN; i++) {
            if (tids[i] == self)
                continue;
            int known = 0;
            for (int k = 0; k < f->n; k++)
                if (f->tid[k] == tids[i]) {
                    known = 1;
                    break;
                }
            if (known)
                continue;
            HANDLE th = OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_SET_CONTEXT |
                                       THREAD_QUERY_LIMITED_INFORMATION,
                                   FALSE, tids[i]);
            if (!th)
                continue;
            if (SuspendThread(th) == (DWORD)-1) {
                CloseHandle(th);
                continue;
            }
            int k = f->n++;
            f->tid[k] = tids[i];
            f->h[k] = th;
            /* GetThreadContext returns only once the thread really stopped */
            memset(&f->ctx[k], 0, sizeof(CONTEXT));
            f->ctx[k].ContextFlags = CONTEXT_CONTROL;
            f->ctx_ok[k] = GetThreadContext(th, &f->ctx[k]) ? 1 : 0;
            added++;
        }
        if (!added)
            break;
    }
    VirtualFree(tids, 0, MEM_RELEASE);
}

static void thaw(frozen *f)
{
    for (int i = 0; i < f->n; i++)
        ResumeThread(f->h[i]);
    for (int i = 0; i < f->n; i++)
        CloseHandle(f->h[i]);
    f->n = 0;
}

#ifdef _WIN64
#define CTX_IP(c) ((c)->Rip)
#else
#define CTX_IP(c) ((c)->Eip)
#endif

/* ------------------------------------------------------------------ commit */

/* Write the 5-byte jmp. One atomic compare-exchange where the bytes fit an
 * aligned 8-byte (or, on x64, 16-byte) window, so even a thread we could not
 * suspend sees old or new code, never a mix. */
static int write_patch(nvt_hook *h)
{
    uint8_t *t = (uint8_t *)h->target;
    int32_t rel32 = (int32_t)((intptr_t)h->entry - ((intptr_t)t + 5));
    uint8_t jmp[5];
    jmp[0] = 0xE9;
    memcpy(jmp + 1, &rel32, 4);

    uintptr_t a8 = (uintptr_t)t & ~(uintptr_t)7;
    size_t o8 = (uintptr_t)t - a8;
#ifdef _WIN64
    uintptr_t a16 = (uintptr_t)t & ~(uintptr_t)15;
    size_t o16 = (uintptr_t)t - a16;
#endif
    uint8_t *pbase = t;
    size_t plen = h->consumed;
    if (o8 + 5 <= 8) {
        pbase = (uint8_t *)a8;
        plen = 8;
    }
#ifdef _WIN64
    else if (o16 + 5 <= 16) {
        pbase = (uint8_t *)a16;
        plen = 16;
    }
#endif
    DWORD oldprot;
    if (!VirtualProtect(pbase, plen, PAGE_EXECUTE_READWRITE, &oldprot))
        return NTR_HOOKERR_PROTECT;
    int rc = 0;
    if (memcmp(t, h->orig_bytes, h->consumed)) {
        rc = NTR_HOOKERR_CHANGED;      /* someone else patched it meanwhile */
    } else if (o8 + 5 <= 8) {
        LONG64 oldv, newv;
        memcpy(&oldv, (void *)a8, 8);
        newv = oldv;
        memcpy((uint8_t *)&newv + o8, jmp, 5);
        if (InterlockedCompareExchange64((volatile LONG64 *)a8, newv, oldv) != oldv)
            rc = NTR_HOOKERR_CHANGED;
    }
#ifdef _WIN64
    else if (o16 + 5 <= 16) {
        uint64_t cmp[2], nv[2];
        memcpy(cmp, (void *)a16, 16);
        memcpy(nv, cmp, 16);
        memcpy((uint8_t *)nv + o16, jmp, 5);
        uint8_t okc;
        __asm__ volatile("lock cmpxchg16b %1\n\tsetz %0"
                         : "=q"(okc), "+m"(*(volatile uint64_t(*)[2])a16), "+a"(cmp[0]), "+d"(cmp[1])
                         : "b"(nv[0]), "c"(nv[1])
                         : "memory", "cc");
        if (!okc)
            rc = NTR_HOOKERR_CHANGED;
    }
#endif
    else {
        memcpy(t, jmp, 5);              /* other threads are suspended */
    }
    VirtualProtect(pbase, plen, oldprot, &oldprot);
    FlushInstructionCache(GetCurrentProcess(), pbase, plen);
    return rc;
}

int nvt_hooks_commit(nvt_hook **hs, int n, uint32_t *errs, uint32_t *moved)
{
    frozen *f = VirtualAlloc(NULL, sizeof(frozen), MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE);
    int ok = 0;
    *moved = 0;
    if (f)
        freeze_others(f);
    /* --- other threads stopped: no heap, no locks, no logging from here */
    for (int i = 0; i < n; i++)
        errs[i] = 0;
    /* first move every thread that stopped inside a prologue about to be
     * patched to the same instruction in the trampoline (valid before the
     * patch too: the trampoline ends with a jump past it). A thread that
     * cannot be moved keeps that function unhooked. */
    for (int k = 0; f && k < f->n; k++) {
        if (!f->ctx_ok[k]) {
            memset(&f->ctx[k], 0, sizeof(CONTEXT));
            f->ctx[k].ContextFlags = CONTEXT_CONTROL;
            f->ctx_ok[k] = GetThreadContext(f->h[k], &f->ctx[k]) ? 1 : 0;
            if (!f->ctx_ok[k])
                continue;   /* unknown IP: the 5-byte window is a tiny target */
        }
        uintptr_t ip = (uintptr_t)CTX_IP(&f->ctx[k]);
        for (int i = 0; i < n; i++) {
            nvt_hook *h = hs[i];
            uintptr_t t = (uintptr_t)h->target;
            if (ip <= t || ip >= t + h->consumed)
                continue;
            int d = x86_relocmap_lookup(&h->relmap, (int)(ip - t));
            CONTEXT c2 = f->ctx[k];
            CTX_IP(&c2) = (uintptr_t)h->tramp + (uintptr_t)(d < 0 ? 0 : d);
            c2.ContextFlags = CONTEXT_CONTROL;
            if (d >= 0 && SetThreadContext(f->h[k], &c2))
                (*moved)++;
            else
                errs[i] = NTR_HOOKERR_BUSY;
            break;
        }
    }
    for (int i = 0; i < n; i++) {
        if (!errs[i])
            errs[i] = (uint32_t)write_patch(hs[i]);
        if (!errs[i]) {
            hs[i]->installed = 1;
            ok++;
        }
    }
    if (f) {
        thaw(f);
        VirtualFree(f, 0, MEM_RELEASE);
    }
    return ok;
}
