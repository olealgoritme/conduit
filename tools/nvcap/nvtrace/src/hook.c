/* Inline hooks: an x86/x64 instruction length decoder, prologue relocation
 * into a trampoline, and the patch at the target.
 *
 * x64 target patch: jmp qword [rip+0]; dq entry_stub (14 bytes).
 * x86 target patch: jmp rel32 (5 bytes).
 * Entry stub: mov rax/eax, <nvt_hook*>; jmp nvt_entry_common.
 * Trampoline: the relocated whole instructions covering the patch, then a jump
 * back to the first untouched instruction. On x64 the trampoline lives within
 * +-2 GiB of the target so RIP-relative operands can be re-encoded.
 */
#include "nvtrace.h"
#include <string.h>
#include "x86dec.h"

#ifdef _WIN64
#define IS64 1
#else
#define IS64 0
#endif
/* jmp rel32 to the entry stub (x64: the stub is allocated within +-2 GiB) */
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

/* ------------------------------------------------------------------ memory */

/* Executable memory nearaddr `nearaddr` (x64: within +-2 GiB). Simple bump allocator
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
    (void)a; (void)b;
    return 1;
#endif
}

static uint8_t *alloc_block_near(uintptr_t nearaddr)
{
#ifdef _WIN64
    SYSTEM_INFO si;
    GetSystemInfo(&si);
    uintptr_t gran = si.dwAllocationGranularity;
    uintptr_t lo = nearaddr > 0x70000000u ? nearaddr - 0x70000000u : (uintptr_t)si.lpMinimumApplicationAddress;
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
    for (int i = 0; i < nregions; i++) {
        region *g = &regions[i];
        if (g->size - g->used >= n && in_range((uintptr_t)g->base, nearaddr) &&
            in_range((uintptr_t)g->base + g->size, nearaddr)) {
            r = g->base + g->used;
            g->used += n;
            return r;
        }
    }
    if (nregions == MAX_REGIONS)
        return NULL;
    uint8_t *b = alloc_block_near(nearaddr);
    if (!b)
        return NULL;
    memset(b, 0xCC, 65536);
    regions[nregions].base = b;
    regions[nregions].size = 65536;
    regions[nregions].used = n;
    nregions++;
    return b;
}

void nvt_hooks_begin(void)
{
    AcquireSRWLockExclusive(&exec_lock);
}

void nvt_hooks_end(void)
{
    ReleaseSRWLockExclusive(&exec_lock);
}

/* ------------------------------------------------------------------ install */

int nvt_insn_len(const uint8_t *p, int is64)
{
    x86_insn d;
    if (x86_decode(p, is64, &d))
        return -1;
    return d.len;
}

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

int nvt_hook_install(nvt_hook *h, uint32_t *err)
{
    uint8_t *t = (uint8_t *)h->target;
    uint8_t *mem, *entry, *tramp, *o;
    int consumed, tlen;
    DWORD oldprot;

    *err = 0;
    mem = exec_alloc((uintptr_t)t, STUB_MEM);
    if (!mem) {
        *err = NTR_HOOKERR_ALLOC;
        return -1;
    }
    entry = mem;
    tramp = mem + 32;

    /* entry stub: mov rax/eax, <nvt_hook*>; jmp nvt_entry_common */
    o = entry;
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

    /* a target that already starts with a jump was patched by someone else
     * (or by us): relocating it could jump back into our own patch */
    if (t[0] == 0xE9 || t[0] == 0xEB || (t[0] == 0xFF && t[1] == 0x25)) {
        *err = NTR_HOOKERR_DECODE;
        return -1;
    }
    intptr_t rel = (intptr_t)entry - ((intptr_t)t + 5);
    if (rel > INT32_MAX || rel < INT32_MIN) {
        *err = NTR_HOOKERR_ALLOC;
        return -1;
    }
    consumed = x86_relocate(t, (uintptr_t)t, PATCH_LEN, tramp, (uintptr_t)tramp, IS64, &tlen);
    if (consumed < 0 || 32 + tlen + 14 > STUB_MEM) {
        *err = NTR_HOOKERR_DECODE;
        return -1;
    }
    emit_jmp(tramp + tlen, (uintptr_t)t + consumed);
    FlushInstructionCache(GetCurrentProcess(), mem, STUB_MEM);

    int aligned = ((uintptr_t)t & 7) == 0;
    size_t span = aligned ? 8 : (size_t)consumed;
    if (span < (size_t)consumed)
        span = (size_t)consumed;
    if (!VirtualProtect(t, span, PAGE_EXECUTE_READWRITE, &oldprot)) {
        *err = NTR_HOOKERR_PROTECT;
        return -1;
    }
    h->orig = tramp;
    MemoryBarrier();
    /* jmp rel32 over the first 5 bytes; the rest of the last relocated
     * instruction is never executed (the trampoline resumes after it). An
     * 8-byte aligned target is patched with one atomic store, so a thread
     * running the function meanwhile sees either the old or the new code. */
    uint8_t patch[8];
    int32_t rel32 = (int32_t)rel;
    memcpy(patch, t, 8);
    patch[0] = 0xE9;
    memcpy(patch + 1, &rel32, 4);
    if (aligned) {
        LONG64 oldv, newv;
        memcpy(&oldv, t, 8);
        memcpy(&newv, patch, 8);
        if (InterlockedCompareExchange64((volatile LONG64 *)t, newv, oldv) != oldv) {
            VirtualProtect(t, span, oldprot, &oldprot);
            *err = NTR_HOOKERR_PROTECT;
            return -1;
        }
    } else {
        memcpy(t, patch, 5);
    }
    VirtualProtect(t, span, oldprot, &oldprot);
    FlushInstructionCache(GetCurrentProcess(), t, span);
    return 0;
}
