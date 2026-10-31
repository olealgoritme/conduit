/*
 * Argument capture for every wrapped call.
 *
 * nvddi_pre():  CALL record = argument values + the records/scalars they point
 *               to (level >= 2), then nested buffers found by the capture rules
 *               of each pointed-to record (level 3) as BLOB records.
 * nvddi_post(): RET record = return value + argument values + every non-const
 *               pointee again, then POST rules (incl. "written" DMA ranges).
 *
 * All nested reads take pointers and lengths from the copy already in the
 * ring (never from the live structure twice), go through safe_copy(), and
 * only happen at <= DISPATCH_LEVEL.  Above DISPATCH_LEVEL (interrupt
 * routines, DxgkCbNotifyInterrupt) only small argument records in kernel
 * memory are copied, directly, because the callee reads them anyway.
 */
#include "nvddi.h"

#define MAX_PLAN        16
#define HIGH_IRQL_MAX   1024
#define MAX_ELEMS       256
#define MAX_PTRARRAY    64
#define MAX_WRITTEN     (64u * 1024 * 1024)

typedef struct PLAN {
    ULONG_PTR addr;
    ULONG     len;
    ULONG     orig;
    USHORT    kind;
    USHORT    arg;
    USHORT    flags;
    USHORT    direct;
} PLAN;

static BOOLEAN read_field(const UCHAR *snap, ULONG snaplen, ULONG off, ULONG size, ULONG64 *out)
{
    ULONG64 v = 0;
    if (!size || size > 8 || off + size > snaplen)
        return FALSE;
    RtlCopyMemory(&v, snap + off, size);
    *out = v;
    return TRUE;
}

static BOOLEAN cond_ok(const NVDDI_RULE *r, const UCHAR *snap, ULONG snaplen)
{
    ULONG64 v;
    if (r->cond_size) {
        if (!read_field(snap, snaplen, r->cond_off, r->cond_size, &v) || (ULONG)v != r->cond_val)
            return FALSE;
    }
    if (r->cond2_size) {
        if (!read_field(snap, snaplen, r->cond2_off, r->cond2_size, &v) || (ULONG)v != r->cond2_val)
            return FALSE;
    }
    return TRUE;
}

static ULONG cap(ULONG64 want, USHORT *flags)
{
    if (want > g.max_blob) {
        *flags |= NDL_BF_TRUNC;
        return g.max_blob;
    }
    return (ULONG)want;
}

static void fill_blob(NDL_BLOB *b, ULONG len, ULONG64 orig, USHORT kind, USHORT arg, USHORT rule, USHORT flags,
                      ULONG elem, USHORT parent, USHORT depth, ULONG_PTR addr)
{
    b->len = len;
    b->orig_len = orig > 0xFFFFFFFFull ? 0xFFFFFFFFu : (ULONG)orig;
    b->kind = kind;
    b->arg = arg;
    b->rule = rule;
    b->flags = flags;
    b->elem = elem;
    b->parent_rule = parent;
    b->depth = depth;
    b->addr = addr;
}

/* One nested buffer as its own BLOB record (left uncommitted: *out_rec). */
static UCHAR *emit_nested(NVDDI_CALL *c, ULONG64 owner, USHORT kind, USHORT rule, USHORT parent, ULONG elem,
                          USHORT depth, USHORT arg, ULONG_PTR addr, ULONG64 want, USHORT flags, BOOLEAN direct,
                          NDL_REC **out_rec, ULONG *out_len)
{
    NDL_REC *rec;
    NDL_BLOB *b;
    UCHAR *data;
    ULONG len = cap(want, &flags);
    ULONG got;
    KIRQL irql = KeGetCurrentIrql();
    UNREFERENCED_PARAMETER(c);

    rec = ring_reserve(sizeof(NDL_REC) + 8 + sizeof(NDL_BLOB) + ALIGN8(len), NDL_T_BLOB);
    if (!rec)
        return NULL;
    *(ULONG64 *)(rec + 1) = owner;
    b = (NDL_BLOB *)((UCHAR *)(rec + 1) + 8);
    data = (UCHAR *)(b + 1);
    got = safe_copy(data, addr, len, irql, direct, &flags);
    if (ALIGN8(len) > len)
        RtlZeroMemory(data + len, ALIGN8(len) - len);
    fill_blob(b, len, want, kind, arg, rule, flags, elem, parent, depth, addr);
    *out_rec = rec;
    *out_len = got;
    return data;
}

static void run_rules(NVDDI_CALL *c, ULONG64 owner, USHORT rsid, const UCHAR *snap, ULONG snaplen, UCHAR phase,
                      USHORT parent, ULONG elem, USHORT depth, USHORT arg);

static void apply_rule(NVDDI_CALL *c, ULONG64 owner, const NVDDI_RULE *r, const UCHAR *snap, ULONG snaplen,
                       UCHAR phase, USHORT parent, ULONG elem, USHORT depth, USHORT arg)
{
    ULONG64 ptr = 0, n = 0, m = 0, want = 0;
    ULONG_PTR addr;
    USHORT flags = (phase == W_POST) ? NDL_BF_POST : 0;
    BOOLEAN direct = FALSE;
    NDL_REC *rec = NULL;
    ULONG got = 0;
    UCHAR *data;
    KIRQL irql = KeGetCurrentIrql();

    if (r->kind == RK_WRITTEN) {
        ULONG k;
        if (depth != 0 || !read_field(snap, snaplen, r->ptr_off, 8, &ptr))
            return;
        if (phase == W_PRE) {
            if (c->nsaved < NVDDI_MAX_SAVED) {
                c->saved_rule[c->nsaved] = r->id;
                c->saved[c->nsaved++] = ptr;
            }
            return;
        }
        for (k = 0; k < c->nsaved; k++)
            if (c->saved_rule[k] == r->id)
                break;
        if (k == c->nsaved || ptr <= c->saved[k] || ptr - c->saved[k] > MAX_WRITTEN)
            return;
        addr = (ULONG_PTR)c->saved[k];
        want = ptr - c->saved[k];
        direct = TRUE;
    } else {
        if (!(r->when & phase) || !cond_ok(r, snap, snaplen))
            return;
        if (r->kind != RK_ADL && !read_field(snap, snaplen, r->ptr_off, 8, &ptr))
            return;
        switch (r->kind) {
        case RK_BYTES:
            if (!read_field(snap, snaplen, r->len_off, r->len_size, &n))
                return;
            want = n;
            break;
        case RK_COUNT:
        case RK_PTRARRAY:
            if (!read_field(snap, snaplen, r->len_off, r->len_size, &n))
                return;
            if (r->maxcount && n > r->maxcount)
                n = r->maxcount;
            if (n > 0x1000000)
                n = 0x1000000;
            want = n * (r->kind == RK_PTRARRAY ? sizeof(void *) : r->elem_size);
            break;
        case RK_ONE:
            want = (ULONG64)r->elem_size * (r->mult ? r->mult : 1);
            break;
        case RK_PRODUCT:
            if (!read_field(snap, snaplen, r->len_off, r->len_size, &n) ||
                !read_field(snap, snaplen, r->end_off, r->end_size, &m))
                return;
            if (n > 0x10000 || m > 0x100000)
                return;
            want = n * m * (r->mult ? r->mult : 1);
            break;
        case RK_RANGE:
            if (!read_field(snap, snaplen, r->len_off, r->len_size, &n) ||
                !read_field(snap, snaplen, r->end_off, r->end_size, &m) || m <= n)
                return;
            ptr += n;
            want = m - n;
            direct = TRUE;
            break;
        case RK_MDL: {
            MDL hdr;
            USHORT f2 = 0;
            if (!ptr || safe_copy(&hdr, (ULONG_PTR)ptr, sizeof(hdr), irql, FALSE, &f2) != sizeof(hdr))
                return;
            want = (hdr.Size >= sizeof(MDL) && hdr.Size <= 16u * 1024 * 1024) ? hdr.Size : sizeof(MDL);
            break;
        }
        case RK_ADL: {
            ULONG64 cnt = 0, fl = 0;
            if (!read_field(snap, snaplen, r->ptr_off, 4, &cnt) ||
                !read_field(snap, snaplen, r->ptr_off + 4, 4, &fl) ||
                !read_field(snap, snaplen, r->ptr_off + 8, 8, &ptr))
                return;
            if (fl & 1)          /* DXGK_ADL_FLAGS.Contiguous: union holds BasePageNumber */
                return;
            want = cnt * sizeof(ULONG64);
            break;
        }
        default:
            return;
        }
        addr = (ULONG_PTR)ptr;
    }
    if (!addr || !want)
        return;
    data = emit_nested(c, owner, NDL_B_NEST, r->id, parent, elem, depth, arg, addr, want, flags, direct, &rec,
                       &got);
    if (!data)
        return;
    if (r->kind == RK_PTRARRAY) {
        ULONG j, cnt = got / sizeof(void *);
        for (j = 0; j < cnt && j < MAX_PTRARRAY; j++) {
            ULONG_PTR ep = ((ULONG_PTR *)data)[j];
            NDL_REC *erec = NULL;
            ULONG egot = 0;
            UCHAR *ed;
            if (!ep)
                continue;
            ed = emit_nested(c, owner, NDL_B_ELEM, r->id, parent, j, (USHORT)(depth + 1), arg, ep, r->elem_size,
                             flags, FALSE, &erec, &egot);
            if (!ed)
                continue;
            if (r->sub != 0xFFFF && depth < 2)
                run_rules(c, owner, r->sub, ed, egot, phase, r->id, j, (USHORT)(depth + 1), arg);
            ring_commit(erec);
        }
    } else if (r->sub != 0xFFFF && depth < 2 && (r->kind == RK_COUNT || r->kind == RK_ONE) && r->elem_size) {
        ULONG j, cnt = got / r->elem_size;
        for (j = 0; j < cnt && j < MAX_ELEMS; j++)
            run_rules(c, owner, r->sub, data + (ULONG64)j * r->elem_size, r->elem_size, phase, r->id, j,
                      (USHORT)(depth + 1), arg);
    }
    ring_commit(rec);
}

static void run_rules(NVDDI_CALL *c, ULONG64 owner, USHORT rsid, const UCHAR *snap, ULONG snaplen, UCHAR phase,
                      USHORT parent, ULONG elem, USHORT depth, USHORT arg)
{
    const NVDDI_RULESET *rs;
    ULONG k;
    if (rsid >= g_nvddi_nrulesets)
        return;
    rs = &g_nvddi_rulesets[rsid];
    for (k = 0; k < rs->n; k++)
        apply_rule(c, owner, &g_nvddi_rules[rs->first + k], snap, snaplen, phase, parent, elem, depth, arg);
}

/* read a pointer-sized out value (AK_PPREC) */
static ULONG_PTR read_ptr(ULONG_PTR p, KIRQL irql)
{
    ULONG_PTR v = 0;
    USHORT f = 0;
    if (!p)
        return 0;
    if (irql > DISPATCH_LEVEL) {
        if (!is_kernel_address(p))
            return 0;
        return *(volatile ULONG_PTR *)p;
    }
    safe_copy(&v, p, sizeof(v), irql, FALSE, &f);
    return v;
}

static void record_call(NVDDI_CALL *c, ULONG_PTR *av, UCHAR phase, ULONG64 ret)
{
    const NVDDI_SLOT *s = c->s;
    KIRQL irql = KeGetCurrentIrql();
    BOOLEAN high = irql > DISPATCH_LEVEL;
    PLAN pl[MAX_PLAN];
    const UCHAR *snap[MAX_PLAN];
    ULONG snaplen[MAX_PLAN];
    ULONG np = 0, i, size, n = s->nargs;
    NDL_REC *rec;
    NDL_CALL *call;
    ULONG64 *args;
    UCHAR *p;

    size = sizeof(NDL_REC) + sizeof(NDL_CALL) + 8 * n;
    if (c->level >= NVDDI_LEVEL_STRUCTS) {
        for (i = 0; i < n && np < MAX_PLAN; i++) {
            const NVDDI_ARG *a = &s->a[i];
            ULONG_PTR ptr = phase ? av[i] : c->pre[i];
            ULONG64 len = 0;
            BOOLEAN is_const = (a->flags & AF_CONST) != 0;
            switch (a->kind) {
            case AK_PREC:
            case AK_PTABLE:
                if (phase && ((is_const && !(a->flags & AF_POSTRULES)) || a->kind == AK_PTABLE))
                    continue;
                len = (a->flags & AF_BUF) ? (ULONG)av[a->lenarg] : a->size;
                if ((a->flags & AF_SIZE16) && ptr && !high) {
                    USHORT sz16 = 0, f16 = 0;
                    if (safe_copy(&sz16, ptr, sizeof(sz16), irql, FALSE, &f16) == sizeof(sz16) && sz16 > len)
                        len = sz16;
                }
                break;
            case AK_PSCALAR:
                if (phase && is_const)
                    continue;
                len = (a->flags & AF_BUF) ? (ULONG)av[a->lenarg] : a->size;
                break;
            case AK_PVOID:
                if (!(a->flags & AF_BUF) || (phase && is_const))
                    continue;
                len = (ULONG)av[a->lenarg];
                break;
            case AK_PPREC:
            case AK_PPTABLE:
                if (!phase || (LONG)ret < 0)
                    continue;
                ptr = read_ptr(ptr, irql);
                len = a->size;
                break;
            default:
                continue;
            }
            if (!ptr || !len)
                continue;
            pl[np].addr = ptr;
            pl[np].orig = len > 0xFFFFFFFFull ? 0xFFFFFFFFu : (ULONG)len;
            pl[np].flags = phase ? NDL_BF_POST : 0;
            pl[np].kind = (a->kind == AK_PVOID) ? NDL_B_LENBUF : NDL_B_ARG;
            pl[np].arg = (USHORT)i;
            pl[np].direct = 0;
            if (high) {
                if (!is_kernel_address(ptr) || len > HIGH_IRQL_MAX) {
                    pl[np].flags |= NDL_BF_IRQL;
                    len = 0;
                } else {
                    pl[np].direct = 1;
                }
            }
            pl[np].len = cap(len, &pl[np].flags);
            size += sizeof(NDL_BLOB) + ALIGN8(pl[np].len);
            np++;
        }
    }
    /* a record must fit in half a ring: cut the largest buffers (flagged TRUNC) */
    while (size > g.ring_bytes / 2 - 4096 && np) {
        ULONG big = 0, k, excess = size - (g.ring_bytes / 2 - 4096);
        for (k = 1; k < np; k++)
            if (pl[k].len > pl[big].len)
                big = k;
        if (!pl[big].len)
            break;
        size -= ALIGN8(pl[big].len);
        pl[big].len = pl[big].len > excess ? (pl[big].len - excess) & ~7u : 0;
        size += ALIGN8(pl[big].len);
        pl[big].flags |= NDL_BF_TRUNC;
    }
    rec = ring_reserve(size, phase ? NDL_T_RET : NDL_T_CALL);
    if (!rec)
        return;
    call = (NDL_CALL *)(rec + 1);
    call->table = s->table;
    call->slot = s->index;
    call->nargs = (UCHAR)n;
    call->phase = phase;
    call->nblobs = (USHORT)np;
    call->call_seq = phase ? c->seq : 0;
    call->ret = ret;
    args = (ULONG64 *)(call + 1);
    for (i = 0; i < n; i++)
        args[i] = phase ? av[i] : c->pre[i];
    p = (UCHAR *)(args + n);
    for (i = 0; i < np; i++) {
        NDL_BLOB *b = (NDL_BLOB *)p;
        UCHAR *data = (UCHAR *)(b + 1);
        USHORT flags = pl[i].flags;
        ULONG got = 0;
        if (pl[i].len) {
            if (pl[i].direct) {
                RtlCopyMemory(data, (const void *)pl[i].addr, pl[i].len);
                got = pl[i].len;
            } else {
                got = safe_copy(data, pl[i].addr, pl[i].len, irql, FALSE, &flags);
            }
        }
        if (ALIGN8(pl[i].len) > pl[i].len)
            RtlZeroMemory(data + pl[i].len, ALIGN8(pl[i].len) - pl[i].len);
        fill_blob(b, pl[i].len, pl[i].orig, pl[i].kind, pl[i].arg, 0xFFFF, flags, 0xFFFFFFFFu, 0xFFFF, 0,
                  pl[i].addr);
        snap[i] = data;
        snaplen[i] = got;
        p = data + ALIGN8(pl[i].len);
    }
    if (!phase)
        c->seq = rec->seq;
    if (c->level >= NVDDI_LEVEL_FULL && !high) {
        for (i = 0; i < np; i++) {
            const NVDDI_ARG *a = &s->a[pl[i].arg];
            if (pl[i].kind == NDL_B_ARG && a->rs != 0xFFFF && snaplen[i])
                run_rules(c, rec->seq, a->rs, snap[i], snaplen[i], phase ? W_POST : W_PRE, 0xFFFF, 0xFFFFFFFFu, 0,
                          pl[i].arg);
        }
    }
    ring_commit(rec);
}

BOOLEAN slot_muted(const NVDDI_SLOT *s)
{
    if (s->table >= NVDDI_NTABLES || s->index >= 256)
        return FALSE;
    return (g.mute[s->table][s->index >> 5] >> (s->index & 31)) & 1;
}

void nvddi_pre(NVDDI_CALL *c, const NVDDI_SLOT *s, ULONG_PTR *av)
{
    ULONG i, n = s->nargs;
    LONG lvl;

    c->s = s;
    c->seq = 0;
    c->active = 0;
    c->nsaved = 0;
    for (i = 0; i < n; i++)
        c->pre[i] = av[i];
    InterlockedIncrement64(&g.calls);

    /* function tables handed to the miniport (DXGKRNL_INTERFACE at StartDevice) */
    for (i = 0; i < n; i++)
        if (s->a[i].kind == AK_PTABLE && av[i] && KeGetCurrentIrql() <= APC_LEVEL)
            av[i] = (ULONG_PTR)wrap_table(s->a[i].table, (void *)av[i]);

    lvl = g.level;
    if (!lvl || g.state == ST_STOPPED || slot_muted(s))
        return;
    c->active = 1;
    c->level = (UCHAR)lvl;
    c->irql = KeGetCurrentIrql();
    record_call(c, av, 0, 0);
}

void nvddi_post(NVDDI_CALL *c, ULONG_PTR *av, ULONG64 ret)
{
    const NVDDI_SLOT *s = c->s;
    ULONG i;

    if (c->active)
        record_call(c, av, 1, ret);

    /* out-parameters that return dxgkrnl function tables (VidPN, monitor) */
    if (KeGetCurrentIrql() <= APC_LEVEL && (s->retkind == 0 || (LONG)ret >= 0)) {
        for (i = 0; i < s->nargs; i++) {
            if (s->a[i].kind == AK_PPTABLE && av[i]) {
                void **pp = (void **)av[i];
                if (*pp)
                    *pp = wrap_table(s->a[i].table, *pp);
            }
        }
    }

    if (s->table == NVDDI_T_DRIVER_INITIALIZATION_DATA && s->index == NVDDI_S_0_DxgkDdiStartDevice &&
        (LONG)ret < 0) {
        InterlockedExchange(&g.start_failed, 1);
        nvddi_set_fail_count(g.max_fails);
        log_event(NDL_EV_START_FAIL, (NTSTATUS)ret, 0, 0,
                  "hooked DxgkDdiStartDevice failed; nvddi disables itself at next boot");
    }
}
