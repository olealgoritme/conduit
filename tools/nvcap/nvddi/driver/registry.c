/*
 * Registry accesses made by the hooked miniport.
 *
 * nvlddmkm reads its configuration with Zw* registry calls, which bypass the
 * DDI interface.  A documented registry filter callback (CmRegisterCallbackEx)
 * sees every kernel-mode registry operation in the calling thread; the ones
 * whose call stack (RtlCaptureStackBackTrace) contains an address inside the
 * hooked miniport's image are recorded as NDL_T_REGISTRY: key path, value
 * name, type and data (after the call for reads, before it for writes).
 * The callback only observes: it always returns STATUS_SUCCESS.
 */
#include "nvddi.h"

NTKERNELAPI NTSTATUS NTAPI CmCallbackGetKeyObjectIDEx(PLARGE_INTEGER Cookie, PVOID Object, PULONG_PTR ObjectID,
                                                     PCUNICODE_STRING *ObjectName, ULONG Flags);
NTKERNELAPI VOID NTAPI CmCallbackReleaseKeyObjectIDEx(PCUNICODE_STRING ObjectName);
NTSYSAPI USHORT NTAPI RtlCaptureStackBackTrace(ULONG FramesToSkip, ULONG FramesToCapture, PVOID *BackTrace,
                                               PULONG BackTraceHash);

static LARGE_INTEGER s_cookie;
static BOOLEAN s_registered;

static BOOLEAN from_target(void)
{
    PVOID frames[64];
    USHORT n, i;
    ULONG_PTR b = (ULONG_PTR)g.tgt_base, e = b + g.tgt_size;
    if (!b)
        return FALSE;
    n = RtlCaptureStackBackTrace(1, 64, frames, NULL);
    for (i = 0; i < n; i++)
        if ((ULONG_PTR)frames[i] >= b && (ULONG_PTR)frames[i] < e)
            return TRUE;
    return FALSE;
}

static void put_ustr(UCHAR **p, const UNICODE_STRING *s, USHORT len)
{
    if (len)
        RtlCopyMemory(*p, s->Buffer, len);
    *p += len;
}

static void log_reg(ULONG op, NTSTATUS st, PVOID object, PCUNICODE_STRING altname, PCUNICODE_STRING value,
                    ULONG type, ULONG info_class, ULONG index, ULONG_PTR data, ULONG data_len)
{
    PCUNICODE_STRING kname = NULL;
    UNICODE_STRING empty = {0, 0, NULL};
    const UNICODE_STRING *k;
    USHORT kl, vl;
    ULONG len, size;
    USHORT flags = 0;
    NDL_REC *rec;
    NDL_REGISTRY *r;
    UCHAR *p;

    if (object && !NT_SUCCESS(CmCallbackGetKeyObjectIDEx(&s_cookie, object, NULL, &kname, 0)))
        kname = NULL;
    k = kname ? kname : (altname ? altname : &empty);
    if (!value)
        value = &empty;
    kl = (k->Buffer && k->Length <= 2048) ? k->Length : 0;
    vl = (value->Buffer && value->Length <= 1024) ? value->Length : 0;
    len = data ? data_len : 0;
    if (len > g.max_blob) {
        len = g.max_blob;
        flags |= NDL_BF_TRUNC;
    }
    size = sizeof(NDL_REC) + sizeof(NDL_REGISTRY) + ALIGN8((ULONG)kl + vl) + ALIGN8(len);
    rec = ring_reserve(size, NDL_T_REGISTRY);
    if (rec) {
        r = (NDL_REGISTRY *)(rec + 1);
        r->op = op;
        r->status = (ULONG)st;
        r->type = type;
        r->info_class = info_class;
        r->index = index;
        r->key_len = kl;
        r->value_len = vl;
        r->data_len = len;
        r->data_orig = data ? data_len : 0;
        r->flags = flags;
        r->rsv = 0;
        p = (UCHAR *)(r + 1);
        put_ustr(&p, k, kl);
        put_ustr(&p, value, vl);
        RtlZeroMemory(p, ALIGN8((ULONG)kl + vl) - (kl + vl));
        p += ALIGN8((ULONG)kl + vl) - (kl + vl);
        if (len) {
            safe_copy(p, data, len, KeGetCurrentIrql(), FALSE, &r->flags);
            RtlZeroMemory(p + len, ALIGN8(len) - len);
        }
        ring_commit(rec);
    }
    if (kname)
        CmCallbackReleaseKeyObjectIDEx(kname);
}

static NTSTATUS reg_cb(PVOID ctx, PVOID a1, PVOID a2)
{
    REG_NOTIFY_CLASS cls = (REG_NOTIFY_CLASS)(ULONG_PTR)a1;
    UNREFERENCED_PARAMETER(ctx);

    if (!g.log_registry || !g.tgt_base || !g.level || g.state == ST_STOPPED || !a2)
        return STATUS_SUCCESS;
    if (KeGetCurrentIrql() != PASSIVE_LEVEL || ExGetPreviousMode() != KernelMode)
        return STATUS_SUCCESS;
    switch (cls) {
    case RegNtPostQueryValueKey:
    case RegNtPostEnumerateValueKey:
    case RegNtPostOpenKeyEx:
    case RegNtPostCreateKeyEx:
    case RegNtPreSetValueKey:
    case RegNtPreDeleteValueKey:
        break;
    default:
        return STATUS_SUCCESS;
    }
    if (!from_target())
        return STATUS_SUCCESS;

    switch (cls) {
    case RegNtPostQueryValueKey: {
        PREG_POST_OPERATION_INFORMATION po = (PREG_POST_OPERATION_INFORMATION)a2;
        PREG_QUERY_VALUE_KEY_INFORMATION q = (PREG_QUERY_VALUE_KEY_INFORMATION)po->PreInformation;
        ULONG got = 0;
        if (!q)
            break;
        if (NT_SUCCESS(po->Status) && q->ResultLength)
            got = *q->ResultLength;
        if (got > q->Length)
            got = q->Length;
        log_reg(cls, po->Status, po->Object, NULL, q->ValueName, 0, q->KeyValueInformationClass, 0,
                NT_SUCCESS(po->Status) ? (ULONG_PTR)q->KeyValueInformation : 0, got);
        break;
    }
    case RegNtPostEnumerateValueKey: {
        PREG_POST_OPERATION_INFORMATION po = (PREG_POST_OPERATION_INFORMATION)a2;
        PREG_ENUMERATE_VALUE_KEY_INFORMATION q = (PREG_ENUMERATE_VALUE_KEY_INFORMATION)po->PreInformation;
        ULONG got = 0;
        if (!q)
            break;
        if (NT_SUCCESS(po->Status) && q->ResultLength)
            got = *q->ResultLength;
        if (got > q->Length)
            got = q->Length;
        log_reg(cls, po->Status, po->Object, NULL, NULL, 0, q->KeyValueInformationClass, q->Index,
                NT_SUCCESS(po->Status) ? (ULONG_PTR)q->KeyValueInformation : 0, got);
        break;
    }
    case RegNtPostOpenKeyEx:
    case RegNtPostCreateKeyEx: {
        PREG_POST_OPERATION_INFORMATION po = (PREG_POST_OPERATION_INFORMATION)a2;
        PREG_CREATE_KEY_INFORMATION q = (PREG_CREATE_KEY_INFORMATION)po->PreInformation;
        log_reg(cls, po->Status, NT_SUCCESS(po->Status) ? po->Object : NULL, q ? q->CompleteName : NULL, NULL,
                0, 0, 0, 0, 0);
        break;
    }
    case RegNtPreSetValueKey: {
        PREG_SET_VALUE_KEY_INFORMATION q = (PREG_SET_VALUE_KEY_INFORMATION)a2;
        log_reg(cls, STATUS_PENDING, q->Object, NULL, q->ValueName, q->Type, 0, 0, (ULONG_PTR)q->Data,
                q->DataSize);
        break;
    }
    case RegNtPreDeleteValueKey: {
        PREG_DELETE_VALUE_KEY_INFORMATION q = (PREG_DELETE_VALUE_KEY_INFORMATION)a2;
        log_reg(cls, STATUS_PENDING, q->Object, NULL, q->ValueName, 0, 0, 0, 0, 0);
        break;
    }
    default:
        break;
    }
    return STATUS_SUCCESS;
}

NTSTATUS registry_init(void)
{
    UNICODE_STRING alt;
    NTSTATUS st;
    if (!g.log_registry || s_registered)
        return STATUS_SUCCESS;
    RtlInitUnicodeString(&alt, L"385201.5");
    st = CmRegisterCallbackEx(reg_cb, &alt, g.driver, NULL, &s_cookie, NULL);
    if (NT_SUCCESS(st))
        s_registered = TRUE;
    return st;
}
