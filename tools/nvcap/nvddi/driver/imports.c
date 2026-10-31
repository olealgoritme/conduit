/*
 * Non-DDI kernel imports made by the hooked miniport.
 *
 * nvlddmkm maps BAR space, allocates contiguous/physical memory, builds MDLs,
 * queues DPCs, connects its ISR and reads the registry directly through
 * ntoskrnl/hal exports, not through the dxgkrnl DDI.  To record these, nvddi
 * rewrites the miniport's import address table (IAT) in memory after it loads:
 * each slot for a function we log is replaced with a pointer to a wrapper that
 * logs and forwards to the saved original.  Overwriting IAT pointers is a data
 * change to the loaded image's data (the IAT), not a change to any code.
 *
 * IMPORTANT (opt-in; HookImports, default 0):
 *   - The IAT pages of a loaded driver are made read-only by the loader.  nvddi
 *     writes them through a read/write MDL alias (MmMapLockedPagesSpecifyCache),
 *     which only works when the image's pages are not SLAT-protected, i.e. when
 *     HVCI / Memory Integrity is OFF.  With HVCI on, the map or the write fails
 *     and nvddi leaves the IAT untouched and logs IMPORT_SKIP.
 *   - The miniport has Kernel CFG metadata (GuardFlags set, 28k guarded calls).
 *     Indirect calls through the IAT are kCFG-checked.  The wrappers live in
 *     nvddi's own image, whose valid call targets the loader publishes to the
 *     global kCFG bitmap at load, so a call into a wrapper passes the check.
 *     This too holds only with the configuration this tool targets (test
 *     signing, HVCI off).  Documented in README; the feature stays off by
 *     default and never changes any driver code.
 *
 * All wrappers log at the caller's IRQL with the lock-free ring, then tail-call
 * the original, so they add no behaviour beyond the log record.
 */
#include "nvddi.h"

#ifndef POOL_FLAGS
typedef ULONG64 POOL_FLAGS;
#endif

typedef struct _NVDDI_IMPORT_DESCRIPTOR {
    union { ULONG Characteristics; ULONG OriginalFirstThunk; };
    ULONG TimeDateStamp;
    ULONG ForwarderChain;
    ULONG Name;
    ULONG FirstThunk;
} IMAGE_IMPORT_DESCRIPTOR;

typedef PVOID (NTAPI *FN_MmMapIoSpaceEx)(PHYSICAL_ADDRESS, SIZE_T, ULONG);
typedef PVOID (NTAPI *FN_MmMapIoSpace)(PHYSICAL_ADDRESS, SIZE_T, MEMORY_CACHING_TYPE);
typedef VOID (NTAPI *FN_MmUnmapIoSpace)(PVOID, SIZE_T);
typedef PVOID (NTAPI *FN_MmAllocContigSpec)(SIZE_T, PHYSICAL_ADDRESS, PHYSICAL_ADDRESS, PHYSICAL_ADDRESS,
                                            MEMORY_CACHING_TYPE);
typedef PVOID (NTAPI *FN_MmAllocContig)(SIZE_T, PHYSICAL_ADDRESS);
typedef VOID (NTAPI *FN_MmFreeContig)(PVOID);
typedef PMDL (NTAPI *FN_MmAllocPagesMdlEx)(PHYSICAL_ADDRESS, PHYSICAL_ADDRESS, PHYSICAL_ADDRESS, SIZE_T,
                                           MEMORY_CACHING_TYPE, ULONG);
typedef PMDL (NTAPI *FN_IoAllocateMdl)(PVOID, ULONG, BOOLEAN, BOOLEAN, PIRP);
typedef PHYSICAL_ADDRESS (NTAPI *FN_MmGetPhysicalAddress)(PVOID);
typedef BOOLEAN (NTAPI *FN_KeInsertQueueDpc)(PKDPC, PVOID, PVOID);
typedef NTSTATUS (NTAPI *FN_IoConnectInterruptEx)(PVOID);
typedef PVOID (NTAPI *FN_ExAllocatePool2)(POOL_FLAGS, SIZE_T, ULONG);
typedef PVOID (NTAPI *FN_ExAllocatePoolWithTag)(POOL_TYPE, SIZE_T, ULONG);
typedef NTSTATUS (NTAPI *FN_ZwOpenKey)(PHANDLE, ACCESS_MASK, POBJECT_ATTRIBUTES);
typedef NTSTATUS (NTAPI *FN_ZwQueryValueKey)(HANDLE, PUNICODE_STRING, KEY_VALUE_INFORMATION_CLASS, PVOID, ULONG,
                                             PULONG);

static struct {
    void       *orig;     /* saved original */
    void      **slot;     /* IAT slot address (for unwrap) */
} s_imp[24];
static ULONG s_nimp;

static void imp_log(ULONG fn, ULONG64 a, ULONG64 b, ULONG64 c, ULONG64 d, ULONG64 ret)
{
    NDL_REC *rec;
    NDL_IMPORT *im;
    if (!g.level || g.state == ST_STOPPED)
        return;
    rec = ring_reserve(sizeof(NDL_REC) + sizeof(NDL_IMPORT), NDL_T_IMPORT);
    if (!rec)
        return;
    im = (NDL_IMPORT *)(rec + 1);
    im->fn = fn;
    im->rsv = 0;
    im->a = a;
    im->b = b;
    im->c = c;
    im->d = d;
    im->ret = ret;
    ring_commit(rec);
}

static void imp_log_name(ULONG fn, ULONG64 a, ULONG64 ret, PCUNICODE_STRING name)
{
    NDL_REC *rec;
    NDL_IMPORT *im;
    NDL_BLOB *bl;
    USHORT nl = (name && name->Buffer && name->Length <= 512) ? name->Length : 0;
    USHORT f = 0;
    if (!g.level || g.state == ST_STOPPED)
        return;
    rec = ring_reserve(sizeof(NDL_REC) + sizeof(NDL_IMPORT) + (nl ? sizeof(NDL_BLOB) + ALIGN8(nl) : 0),
                       NDL_T_IMPORT);
    if (!rec)
        return;
    im = (NDL_IMPORT *)(rec + 1);
    RtlZeroMemory(im, sizeof(*im));
    im->fn = fn;
    im->a = a;
    im->ret = ret;
    if (nl) {
        bl = (NDL_BLOB *)(im + 1);
        RtlZeroMemory(bl, sizeof(*bl));
        safe_copy(bl + 1, (ULONG_PTR)name->Buffer, nl, KeGetCurrentIrql(), FALSE, &f);
        if (ALIGN8(nl) > nl)
            RtlZeroMemory((UCHAR *)(bl + 1) + nl, ALIGN8(nl) - nl);
        bl->len = nl;
        bl->orig_len = nl;
        bl->kind = NDL_B_ARG;
        bl->rule = 0xFFFF;
        bl->flags = f;
        bl->elem = 0xFFFFFFFF;
        bl->parent_rule = 0xFFFF;
    }
    ring_commit(rec);
}

#define ORIG(i) (s_imp[i].orig)

/* The IAT slot index for each function is assigned in build order below. */
enum { I_MapIoEx, I_MapIo, I_UnmapIo, I_AllocContigSpec, I_AllocContig, I_FreeContig, I_AllocPagesMdl,
       I_AllocMdl, I_GetPhys, I_InsertDpc, I_ConnectIntrEx, I_Pool2, I_PoolTag, I_ZwOpenKey, I_ZwQueryValue,
       I_MAX };

static void *w_MmMapIoSpaceEx(PHYSICAL_ADDRESS p, SIZE_T n, ULONG prot)
{
    void *r = ((FN_MmMapIoSpaceEx)ORIG(I_MapIoEx))(p, n, prot);
    imp_log(NDL_IMP_MmMapIoSpaceEx, (ULONG64)p.QuadPart, n, prot, 0, (ULONG64)(ULONG_PTR)r);
    return r;
}
static void *w_MmMapIoSpace(PHYSICAL_ADDRESS p, SIZE_T n, MEMORY_CACHING_TYPE c)
{
    void *r = ((FN_MmMapIoSpace)ORIG(I_MapIo))(p, n, c);
    imp_log(NDL_IMP_MmMapIoSpace, (ULONG64)p.QuadPart, n, c, 0, (ULONG64)(ULONG_PTR)r);
    return r;
}
static void w_MmUnmapIoSpace(void *va, SIZE_T n)
{
    imp_log(NDL_IMP_MmUnmapIoSpace, (ULONG64)(ULONG_PTR)va, n, 0, 0, 0);
    ((FN_MmUnmapIoSpace)ORIG(I_UnmapIo))(va, n);
}
static void *w_MmAllocContigSpec(SIZE_T n, PHYSICAL_ADDRESS lo, PHYSICAL_ADDRESS hi, PHYSICAL_ADDRESS skip,
                                 MEMORY_CACHING_TYPE c)
{
    void *r = ((FN_MmAllocContigSpec)ORIG(I_AllocContigSpec))(n, lo, hi, skip, c);
    imp_log(NDL_IMP_MmAllocateContiguousMemorySpecifyCache, n, (ULONG64)lo.QuadPart, (ULONG64)hi.QuadPart,
            c, (ULONG64)(ULONG_PTR)r);
    return r;
}
static void *w_MmAllocContig(SIZE_T n, PHYSICAL_ADDRESS hi)
{
    void *r = ((FN_MmAllocContig)ORIG(I_AllocContig))(n, hi);
    imp_log(NDL_IMP_MmAllocateContiguousMemory, n, (ULONG64)hi.QuadPart, 0, 0, (ULONG64)(ULONG_PTR)r);
    return r;
}
static void w_MmFreeContig(void *p)
{
    imp_log(NDL_IMP_MmFreeContiguousMemory, (ULONG64)(ULONG_PTR)p, 0, 0, 0, 0);
    ((FN_MmFreeContig)ORIG(I_FreeContig))(p);
}
static PMDL w_MmAllocPagesMdlEx(PHYSICAL_ADDRESS lo, PHYSICAL_ADDRESS hi, PHYSICAL_ADDRESS skip, SIZE_T n,
                                MEMORY_CACHING_TYPE c, ULONG f)
{
    PMDL r = ((FN_MmAllocPagesMdlEx)ORIG(I_AllocPagesMdl))(lo, hi, skip, n, c, f);
    imp_log(NDL_IMP_MmAllocatePagesForMdlEx, (ULONG64)lo.QuadPart, (ULONG64)hi.QuadPart, n,
            f, (ULONG64)(ULONG_PTR)r);
    return r;
}
static PMDL w_IoAllocateMdl(void *va, ULONG len, BOOLEAN sec, BOOLEAN charge, PIRP irp)
{
    PMDL r = ((FN_IoAllocateMdl)ORIG(I_AllocMdl))(va, len, sec, charge, irp);
    imp_log(NDL_IMP_IoAllocateMdl, (ULONG64)(ULONG_PTR)va, len, 0, 0, (ULONG64)(ULONG_PTR)r);
    return r;
}
static PHYSICAL_ADDRESS w_MmGetPhysicalAddress(void *va)
{
    PHYSICAL_ADDRESS r = ((FN_MmGetPhysicalAddress)ORIG(I_GetPhys))(va);
    imp_log(NDL_IMP_MmGetPhysicalAddress, (ULONG64)(ULONG_PTR)va, 0, 0, 0, (ULONG64)r.QuadPart);
    return r;
}
static BOOLEAN w_KeInsertQueueDpc(PKDPC dpc, PVOID a1, PVOID a2)
{
    BOOLEAN r = ((FN_KeInsertQueueDpc)ORIG(I_InsertDpc))(dpc, a1, a2);
    imp_log(NDL_IMP_KeInsertQueueDpc, (ULONG64)(ULONG_PTR)dpc, (ULONG64)(ULONG_PTR)a1,
            (ULONG64)(ULONG_PTR)a2, 0, r);
    return r;
}
static NTSTATUS w_IoConnectInterruptEx(PVOID params)
{
    NTSTATUS r = ((FN_IoConnectInterruptEx)ORIG(I_ConnectIntrEx))(params);
    imp_log(NDL_IMP_IoConnectInterruptEx, (ULONG64)(ULONG_PTR)params, 0, 0, 0, (ULONG)r);
    return r;
}
static void *w_ExAllocatePool2(POOL_FLAGS fl, SIZE_T n, ULONG tag)
{
    void *r = ((FN_ExAllocatePool2)ORIG(I_Pool2))(fl, n, tag);
    imp_log(NDL_IMP_ExAllocatePool2, (ULONG64)fl, n, tag, 0, (ULONG64)(ULONG_PTR)r);
    return r;
}
static void *w_ExAllocatePoolWithTag(POOL_TYPE t, SIZE_T n, ULONG tag)
{
    void *r = ((FN_ExAllocatePoolWithTag)ORIG(I_PoolTag))(t, n, tag);
    imp_log(NDL_IMP_ExAllocatePoolWithTag, t, n, tag, 0, (ULONG64)(ULONG_PTR)r);
    return r;
}
static NTSTATUS w_ZwOpenKey(PHANDLE h, ACCESS_MASK am, POBJECT_ATTRIBUTES oa)
{
    NTSTATUS r = ((FN_ZwOpenKey)ORIG(I_ZwOpenKey))(h, am, oa);
    imp_log_name(NDL_IMP_ZwOpenKey, (ULONG64)am, (ULONG)r, oa ? oa->ObjectName : NULL);
    return r;
}
static NTSTATUS w_ZwQueryValueKey(HANDLE h, PUNICODE_STRING name, KEY_VALUE_INFORMATION_CLASS cls, PVOID info,
                                  ULONG len, PULONG rl)
{
    NTSTATUS r = ((FN_ZwQueryValueKey)ORIG(I_ZwQueryValue))(h, name, cls, info, len, rl);
    imp_log_name(NDL_IMP_ZwQueryValueKey, cls, (ULONG)r, name);
    return r;
}

static const struct {
    const char *name;
    void       *wrapper;
    ULONG       slot;
} IMPORTS[] = {
    {"MmMapIoSpaceEx", w_MmMapIoSpaceEx, I_MapIoEx},
    {"MmMapIoSpace", w_MmMapIoSpace, I_MapIo},
    {"MmUnmapIoSpace", w_MmUnmapIoSpace, I_UnmapIo},
    {"MmAllocateContiguousMemorySpecifyCache", w_MmAllocContigSpec, I_AllocContigSpec},
    {"MmAllocateContiguousMemory", w_MmAllocContig, I_AllocContig},
    {"MmFreeContiguousMemory", w_MmFreeContig, I_FreeContig},
    {"MmAllocatePagesForMdlEx", w_MmAllocPagesMdlEx, I_AllocPagesMdl},
    {"IoAllocateMdl", w_IoAllocateMdl, I_AllocMdl},
    {"MmGetPhysicalAddress", w_MmGetPhysicalAddress, I_GetPhys},
    {"KeInsertQueueDpc", w_KeInsertQueueDpc, I_InsertDpc},
    {"IoConnectInterruptEx", w_IoConnectInterruptEx, I_ConnectIntrEx},
    {"ExAllocatePool2", w_ExAllocatePool2, I_Pool2},
    {"ExAllocatePoolWithTag", w_ExAllocatePoolWithTag, I_PoolTag},
    {"ZwOpenKey", w_ZwOpenKey, I_ZwOpenKey},
    {"ZwQueryValueKey", w_ZwQueryValueKey, I_ZwQueryValue},
};

/* Write 'value' into *slot without faulting: the loaded driver's IAT page is
   virtually read-only, so map its physical page through a fresh writable alias
   (works only with HVCI off; MmMapIoSpaceEx returns NULL otherwise). */
static NTSTATUS write_slot(void **slot, void *value)
{
    PHYSICAL_ADDRESS pa = MmGetPhysicalAddress(slot);
    ULONG64 pageoff = (ULONG64)(ULONG_PTR)slot & (PAGE_SIZE - 1);
    void *map;
    if (!pa.QuadPart)
        return STATUS_NOT_MAPPED_VIEW;
    if (pageoff + sizeof(void *) > PAGE_SIZE)   /* slot would straddle a page */
        return STATUS_INVALID_ADDRESS;
    map = MmMapIoSpaceEx(pa, sizeof(void *), PAGE_READWRITE);
    if (!map)
        return STATUS_NONE_MAPPED;
    InterlockedExchangePointer((void **)map, value);
    MmUnmapIoSpace(map, sizeof(void *));
    return STATUS_SUCCESS;
}

static int name_eq(const char *a, const char *b)
{
    while (*a && *a == *b) {
        a++;
        b++;
    }
    return *a == *b;
}

/* Walk the miniport's import directory; for ntoskrnl.exe / hal.dll entries that
   match our table, save the original and replace the IAT slot. */
NTSTATUS imports_wrap(PDRIVER_OBJECT drv)
{
    UCHAR *base;
    ULONG peoff, imp_rva, imp_size, i, wrapped = 0, matched = 0;
    IMAGE_IMPORT_DESCRIPTOR *desc;

    if (!g.hook_imports || !drv || !drv->DriverStart)
        return STATUS_NOT_SUPPORTED;
    base = (UCHAR *)drv->DriverStart;
    if (base[0] != 'M' || base[1] != 'Z')
        return STATUS_INVALID_IMAGE_FORMAT;
    peoff = *(ULONG *)(base + 0x3C);
    if (*(ULONG *)(base + peoff) != 0x00004550)
        return STATUS_INVALID_IMAGE_FORMAT;
    /* optional header: PE32+ data directory 1 = import */
    imp_rva = *(ULONG *)(base + peoff + 0x18 + 0x70);
    imp_size = *(ULONG *)(base + peoff + 0x18 + 0x74);
    if (!imp_rva || !imp_size)
        return STATUS_NOT_FOUND;
    for (i = 0; i < I_MAX; i++) {
        s_imp[i].orig = NULL;
        s_imp[i].slot = NULL;
    }
    desc = (IMAGE_IMPORT_DESCRIPTOR *)(base + imp_rva);
    for (; desc->Name; desc++) {
        const char *dll = (const char *)(base + desc->Name);
        ULONG64 *ilt, *iat;
        ULONG k;
        if (!name_eq(dll, "ntoskrnl.exe") && !name_eq(dll, "HAL.dll") && !name_eq(dll, "hal.dll"))
            continue;
        ilt = (ULONG64 *)(base + (desc->OriginalFirstThunk ? desc->OriginalFirstThunk : desc->FirstThunk));
        iat = (ULONG64 *)(base + desc->FirstThunk);
        for (k = 0; ilt[k]; k++) {
            const char *fname;
            if (ilt[k] & 0x8000000000000000ull)
                continue;
            fname = (const char *)(base + (ULONG)ilt[k] + 2);
            for (i = 0; i < sizeof(IMPORTS) / sizeof(IMPORTS[0]); i++) {
                if (!name_eq(fname, IMPORTS[i].name))
                    continue;
                matched++;
                s_imp[IMPORTS[i].slot].orig = (void *)iat[k];
                s_imp[IMPORTS[i].slot].slot = (void **)&iat[k];
                break;
            }
        }
    }
    for (i = 0; i < I_MAX; i++) {
        if (!s_imp[i].slot)
            continue;
        if (NT_SUCCESS(write_slot(s_imp[i].slot, IMPORTS[i].wrapper)))
            wrapped++;
        else {
            s_imp[i].slot = NULL;          /* leave it; nothing to unwrap */
            log_event(NDL_EV_INFO, STATUS_ACCESS_DENIED, i, 0,
                      "import IAT write refused (HVCI on?); %s left unwrapped", IMPORTS[i].name);
        }
    }
    s_nimp = I_MAX;
    log_event(NDL_EV_INFO, 0, matched, wrapped, "import wrap: %u matched, %u IAT slots replaced", matched,
              wrapped);
    return wrapped ? STATUS_SUCCESS : STATUS_UNSUCCESSFUL;
}

void imports_unwrap(void)
{
    ULONG i;
    for (i = 0; i < s_nimp; i++)
        if (s_imp[i].slot && s_imp[i].orig)
            write_slot(s_imp[i].slot, s_imp[i].orig);
}
