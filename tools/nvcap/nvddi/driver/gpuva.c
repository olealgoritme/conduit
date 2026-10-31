/*
 * GPU virtual address resolution for command buffers.
 *
 * SubmitCommandVirtual / SubmitCommandToHwQueue hand dxgkrnl only the GPU
 * virtual address and size of the DMA buffer; the bytes are not an argument.
 * To record them, nvddi reconstructs the GpuMmu mapping from what it has
 * already seen on the same interface:
 *
 *   - segment descriptors from QueryAdapterInfo (QUERYSEGMENT*): per segment
 *     whether it is an aperture / system-memory segment (PTE page address is a
 *     system physical address) or CPU-visible local memory (BAR; map through
 *     the segment's CpuTranslatedAddress).
 *   - leaf page-table entries from BuildPagingBuffer UPDATE_PAGE_TABLE
 *     (PageTableLevel 0): VA page -> DXGK_PTE (segment id + page address).
 *     FirstPteVirtualAddress gives the VA of pPageTableEntries[0].
 *
 * A VA is resolved by looking up its 4-KB (or 64-KB) page, reading the backing
 * physical page (MmCopyMemory MM_COPY_MEMORY_PHYSICAL for system/aperture,
 * MmMapIoSpaceEx for CPU-visible local memory), and copying the command bytes.
 * At PASSIVE_LEVEL this is done inline at submit time; at DISPATCH_LEVEL a
 * snapshot request is queued to the writer's worker (contents may have changed
 * by the time it runs, which the blob notes with a late timestamp).
 *
 * The map is best-effort: only VAs whose leaf PTEs were observed resolve.
 * Entries never freed individually; the whole map is a fixed open-addressed
 * table, oldest generation evicted on collision.  All of this is data derived
 * from logged DDI traffic; it changes no driver behaviour.
 */
#include "nvddi.h"

#define VA_SHIFT        12
#define VA_PAGE         (1ull << VA_SHIFT)
#define PTE_PAGE_MASK   0xFFFFFFFFFFFFF000ull   /* PageAddress: high 52 bits, low 12 zero */
#define MAX_CMD_PAGES   4096                     /* 16 MB per command buffer at 4 KB */

typedef struct VA_ENTRY {
    ULONG64 va_page;        /* GPU VA >> 12, 0 = empty */
    ULONG64 phys;           /* backing physical address (page-aligned) */
    ULONG   segment;        /* DXGK_PTE.Segment */
    ULONG   gen;            /* generation for eviction */
    UCHAR   large;          /* 64 KB page */
    UCHAR   valid;
} VA_ENTRY;

typedef struct SEG_INFO {
    ULONG64 base;           /* BaseAddress */
    ULONG64 cpu_translated; /* CpuTranslatedAddress (BAR physical) for CPU-visible local */
    ULONG64 size;
    ULONG   flags;          /* DXGK_SEGMENTFLAGS.Value */
    UCHAR   known;
} SEG_INFO;

#define SEG_APERTURE   0x001
#define SEG_CPUVISIBLE 0x004

static VA_ENTRY *s_map;
static ULONG     s_map_n;            /* power of two */
static ULONG     s_map_mask;
static SEG_INFO  s_seg[64];
static KSPIN_LOCK s_lock;
static volatile LONG s_gen;
static volatile LONG s_pte_count;

NTSTATUS gpuva_init(void)
{
    ULONG n = 1u << 20;   /* 1M entries = 32 MB, covers 4 GB of 4-KB VA pages */
    KeInitializeSpinLock(&s_lock);
    if (g.gpuva_entries)
        n = g.gpuva_entries;
    while (n & (n - 1))
        n &= n - 1;
    if (n < (1u << 12))
        n = 1u << 12;
    s_map = (VA_ENTRY *)ExAllocatePoolWithTag(NonPagedPoolNx, (SIZE_T)n * sizeof(VA_ENTRY), NVDDI_TAG);
    if (!s_map)
        return STATUS_INSUFFICIENT_RESOURCES;
    RtlZeroMemory(s_map, (SIZE_T)n * sizeof(VA_ENTRY));
    s_map_n = n;
    s_map_mask = n - 1;
    return STATUS_SUCCESS;
}

static ULONG hash_va(ULONG64 page)
{
    page *= 0x9E3779B97F4A7C15ull;
    return (ULONG)(page >> 40) & s_map_mask;
}

void gpuva_note_segment(ULONG id, ULONG flags, ULONG64 base, ULONG64 cpu_translated, ULONG64 size)
{
    if (id >= 64)
        return;
    s_seg[id].flags = flags;
    s_seg[id].base = base;
    s_seg[id].cpu_translated = cpu_translated;
    s_seg[id].size = size;
    s_seg[id].known = 1;
}

/* record one leaf page-table entry: va -> pte (called from the UPDATE_PAGE_TABLE rule) */
void gpuva_note_pte(ULONG64 va, ULONG64 pte_flags, ULONG64 pte_pageaddr)
{
    ULONG64 page = va >> VA_SHIFT;
    ULONG seg = (ULONG)((pte_flags >> 5) & 0x1F);
    UCHAR large = (UCHAR)((pte_flags >> 16) & 1);   /* LargePage bit */
    ULONG i, start;
    KIRQL irql;
    if (!s_map || !(pte_flags & 1))                 /* Valid bit */
        return;
    KeAcquireSpinLock(&s_lock, &irql);
    start = hash_va(page);
    for (i = 0; i < 8; i++) {
        VA_ENTRY *e = &s_map[(start + i) & s_map_mask];
        if (!e->valid || e->va_page == page) {
            e->va_page = page;
            e->phys = pte_pageaddr & PTE_PAGE_MASK;
            e->segment = seg;
            e->large = large;
            e->gen = (ULONG)s_gen;
            e->valid = 1;
            KeReleaseSpinLock(&s_lock, irql);
            InterlockedIncrement(&s_pte_count);
            return;
        }
    }
    /* collision run full: evict the oldest of the probe window */
    {
        VA_ENTRY *old = &s_map[start];
        for (i = 1; i < 8; i++)
            if (s_map[(start + i) & s_map_mask].gen < old->gen)
                old = &s_map[(start + i) & s_map_mask];
        old->va_page = page;
        old->phys = pte_pageaddr & PTE_PAGE_MASK;
        old->segment = seg;
        old->large = large;
        old->gen = (ULONG)s_gen;
        old->valid = 1;
    }
    KeReleaseSpinLock(&s_lock, irql);
}

static BOOLEAN lookup(ULONG64 va, ULONG64 *phys, ULONG *seg, UCHAR *large)
{
    ULONG64 page = va >> VA_SHIFT;
    ULONG start = hash_va(page), i;
    for (i = 0; i < 8; i++) {
        VA_ENTRY *e = &s_map[(start + i) & s_map_mask];
        if (e->valid && e->va_page == page) {
            *phys = e->phys + (va & (VA_PAGE - 1));
            *seg = e->segment;
            *large = e->large;
            return TRUE;
        }
        if (!e->valid)
            break;
    }
    return FALSE;
}

/* copy up to len bytes starting at physical address phys of segment seg */
static ULONG read_phys(void *dst, ULONG64 phys, ULONG seg, ULONG len, USHORT *flags)
{
    KIRQL irql = KeGetCurrentIrql();
    SEG_INFO *s = seg < 64 ? &s_seg[seg] : NULL;
    if (irql > APC_LEVEL) {
        *flags |= NDL_BF_IRQL;
        return 0;
    }
    if (s && s->known && (s->flags & SEG_CPUVISIBLE) && !(s->flags & SEG_APERTURE) && s->cpu_translated) {
        /* CPU-visible local memory: map the BAR window */
        PHYSICAL_ADDRESS pa;
        void *va;
        pa.QuadPart = (LONGLONG)(s->cpu_translated + (phys - s->base));
        va = MmMapIoSpaceEx(pa, len, PAGE_READONLY | PAGE_NOCACHE);
        if (!va) {
            *flags |= NDL_BF_FAILED;
            return 0;
        }
        RtlCopyMemory(dst, va, len);
        MmUnmapIoSpace(va, len);
        *flags |= NDL_BF_DIRECT;
        return len;
    } else {
        MM_COPY_ADDRESS a;
        SIZE_T got = 0;
        NTSTATUS st;
        a.PhysicalAddress.QuadPart = (LONGLONG)phys;
        st = MmCopyMemory(dst, a, len, MM_COPY_MEMORY_PHYSICAL, &got);
        if (!NT_SUCCESS(st) && got == 0)
            *flags |= NDL_BF_FAILED;
        return (ULONG)got;
    }
}

/*
 * Resolve [va, va+size) and emit NDL_B_CMDBUF blobs (one per contiguous
 * physical run, at most MAX_CMD_PAGES pages) attached to owner_seq.
 * Returns the number of bytes resolved.  <= APC_LEVEL.
 */
ULONG gpuva_capture(ULONG64 owner_seq, ULONG64 va, ULONG size, ULONG engine)
{
    ULONG done = 0, pages = 0;
    if (!s_map || !va || !size)
        return 0;
    if (size > MAX_CMD_PAGES * VA_PAGE)
        size = MAX_CMD_PAGES * VA_PAGE;
    while (done < size && pages < MAX_CMD_PAGES) {
        ULONG64 cur = va + done, phys = 0, pstep = VA_PAGE;
        ULONG seg = 0;
        UCHAR large = 0;
        ULONG chunk, got;
        NDL_REC *rec;
        NDL_BLOB *b;
        USHORT flags = NDL_BF_POST;
        if (!lookup(cur, &phys, &seg, &large)) {
            /* unmapped page: record a zero-length marker so the gap is visible */
            rec = ring_reserve(sizeof(NDL_REC) + 8 + sizeof(NDL_BLOB), NDL_T_BLOB);
            if (rec) {
                *(ULONG64 *)(rec + 1) = owner_seq;
                b = (NDL_BLOB *)((UCHAR *)(rec + 1) + 8);
                RtlZeroMemory(b, sizeof(*b));
                b->orig_len = (ULONG)(size - done);
                b->kind = NDL_B_CMDBUF;
                b->arg = (USHORT)engine;
                b->rule = 0xFFFF;
                b->flags = NDL_BF_POST | NDL_BF_FAILED;
                b->elem = pages;
                b->parent_rule = 0xFFFF;
                b->addr = cur;
                ring_commit(rec);
            }
            break;
        }
        pstep = large ? (64ull * 1024) : VA_PAGE;
        pstep -= cur & (pstep - 1);
        chunk = (ULONG)((size - done) < pstep ? (size - done) : pstep);
        if (chunk > g.max_blob)
            chunk = g.max_blob;
        rec = ring_reserve(sizeof(NDL_REC) + 8 + sizeof(NDL_BLOB) + ALIGN8(chunk), NDL_T_BLOB);
        if (!rec)
            break;
        *(ULONG64 *)(rec + 1) = owner_seq;
        b = (NDL_BLOB *)((UCHAR *)(rec + 1) + 8);
        got = read_phys(b + 1, phys, seg, chunk, &flags);
        if (ALIGN8(chunk) > chunk)
            RtlZeroMemory((UCHAR *)(b + 1) + chunk, ALIGN8(chunk) - chunk);
        b->len = chunk;
        b->orig_len = chunk;
        b->kind = NDL_B_CMDBUF;
        b->arg = (USHORT)engine;
        b->rule = (USHORT)seg;           /* segment id, for the decoder */
        b->flags = flags;
        b->elem = pages;
        b->parent_rule = 0xFFFF;
        b->depth = 0;
        b->addr = cur;                   /* GPU VA of this run */
        ring_commit(rec);
        if (!got)
            break;
        done += chunk;
        pages++;
    }
    return done;
}

ULONG gpuva_pte_count(void)
{
    return (ULONG)s_pte_count;
}
