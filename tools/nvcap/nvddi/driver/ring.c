/*
 * Per-CPU record rings.
 *
 * Producers run at any IRQL (up to DIRQL inside interrupt routines) and never
 * block or allocate: a record is reserved with a compare-exchange on the ring
 * head, filled, and published by writing its size word last.  A record never
 * wraps; when it does not fit before the end of the buffer a PAD record fills
 * the gap.  The writer thread (PASSIVE_LEVEL) copies committed records out,
 * zeroes the consumed bytes and then advances the tail, so an unpublished
 * record always reads as size 0.  When a ring is full the record is dropped
 * and counted; the writer later logs an NDL_T_DROP record.
 */
#include "nvddi.h"

NTSTATUS ring_init(ULONG ring_bytes)
{
    ULONG n = KeQueryActiveProcessorCountEx(ALL_PROCESSOR_GROUPS);
    ULONG i;
    if (n == 0)
        n = 1;
    if (n > 256)
        n = 256;
    g.rings = (NVDDI_RING *)ExAllocatePoolWithTag(NonPagedPoolNx, sizeof(NVDDI_RING) * n, NVDDI_TAG);
    if (!g.rings)
        return STATUS_INSUFFICIENT_RESOURCES;
    RtlZeroMemory(g.rings, sizeof(NVDDI_RING) * n);
    for (;;) {
        BOOLEAN ok = TRUE;
        for (i = 0; i < n; i++) {
            g.rings[i].buf = (UCHAR *)ExAllocatePoolWithTag(NonPagedPoolNx, ring_bytes, NVDDI_TAG);
            if (!g.rings[i].buf) {
                ok = FALSE;
                break;
            }
            RtlZeroMemory(g.rings[i].buf, ring_bytes);
            g.rings[i].size = ring_bytes;
            g.rings[i].mask = ring_bytes - 1;
        }
        if (ok)
            break;
        while (i > 0) {
            i--;
            ExFreePoolWithTag(g.rings[i].buf, NVDDI_TAG);
            g.rings[i].buf = NULL;
        }
        if (ring_bytes <= 256 * 1024)
            return STATUS_INSUFFICIENT_RESOURCES;
        ring_bytes >>= 1;
    }
    g.ncpu = n;
    g.ring_bytes = ring_bytes;
    InterlockedExchange(&g.rings_ready, 1);
    return STATUS_SUCCESS;
}

static NVDDI_RING *my_ring(void)
{
    ULONG c = cpu_index();
    return &g.rings[c % g.ncpu];
}

NDL_REC *ring_reserve(ULONG size, USHORT type)
{
    NVDDI_RING *r;
    LONG64 h, t, nh;
    ULONG off, pad;
    NDL_REC *rec;
    KIRQL irql;

    if (!g.rings_ready)
        return NULL;
    r = my_ring();
    size = ALIGN8(size);
    if (size < sizeof(NDL_REC) || size > r->size / 2) {
        InterlockedIncrement64(&r->dropped);
        InterlockedAdd64(&r->dropped_bytes, size);
        return NULL;
    }
    for (;;) {
        h = r->head;
        t = InterlockedCompareExchange64(&r->tail, 0, 0);   /* acquire read */
        off = (ULONG)(h & r->mask);
        pad = (off + size > r->size) ? (r->size - off) : 0;
        nh = h + pad + size;
        if ((ULONG64)(nh - t) > r->size) {
            InterlockedIncrement64(&r->dropped);
            InterlockedAdd64(&r->dropped_bytes, size);
            return NULL;
        }
        if (InterlockedCompareExchange64(&r->head, nh, h) == h)
            break;
    }
    if (pad) {
        NDL_REC *p = (NDL_REC *)(r->buf + off);
        p->type = NDL_T_PAD;
        InterlockedExchange((volatile LONG *)&p->size, (LONG)pad);
        off = 0;
    }
    rec = (NDL_REC *)(r->buf + ((h + pad) & r->mask));
    irql = KeGetCurrentIrql();
    rec->type = type;
    rec->flags = irql > DISPATCH_LEVEL ? NDL_RF_HIGHIRQL : 0;
    rec->seq = (ULONG64)InterlockedIncrement64(&g.seq);
    rec->qpc = (ULONG64)KeQueryPerformanceCounter(NULL).QuadPart;
    rec->thread = (ULONG64)(ULONG_PTR)KeGetCurrentThread();
    if (irql <= DISPATCH_LEVEL) {
        rec->pid = (ULONG)(ULONG_PTR)PsGetCurrentProcessId();
        rec->tid = (ULONG)(ULONG_PTR)PsGetCurrentThreadId();
    } else {
        rec->pid = 0;
        rec->tid = 0;
    }
    rec->cpu = (USHORT)cpu_index();
    rec->irql = irql;
    rec->rsv0 = 0;
    rec->rsv1 = 0;
    /* stash the reserved size in rsv1 until commit (size word must stay 0) */
    rec->rsv1 = size;
    return rec;
}

void ring_commit(NDL_REC *rec)
{
    ULONG size;
    if (!rec)
        return;
    size = rec->rsv1;
    rec->rsv1 = 0;
    InterlockedExchange((volatile LONG *)&rec->size, (LONG)size);   /* release */
}

void log_event(ULONG code, NTSTATUS status, ULONG64 a, ULONG64 b, const char *f, ...)
{
    char text[256];
    ULONG tl = 0, size;
    NDL_REC *rec;
    NDL_EVENT *e;
    va_list ap;

    text[0] = 0;
    if (f) {
        va_start(ap, f);
        fmt_v(text, sizeof(text), f, ap);
        va_end(ap);
        while (tl < sizeof(text) - 1 && text[tl])
            tl++;
    }
    size = sizeof(NDL_REC) + sizeof(NDL_EVENT) + ALIGN8(tl + 1);
    rec = ring_reserve(size, NDL_T_EVENT);
    if (!rec)
        return;
    e = (NDL_EVENT *)(rec + 1);
    e->code = code;
    e->status = (ULONG)status;
    e->a = a;
    e->b = b;
    RtlCopyMemory(e + 1, text, tl);
    RtlZeroMemory((UCHAR *)(e + 1) + tl, ALIGN8(tl + 1) - tl);
    ring_commit(rec);
}
