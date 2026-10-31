/*
 * Interception point.
 *
 * A WDDM miniport's DriverEntry calls displib!DxgkInitialize, which loads the
 * DXGKrnl service, opens \Device\DxgKrnl with IoGetDeviceObjectPointer and
 * sends IRP_MJ_INTERNAL_DEVICE_CONTROL 0x230047 (Win8+; 0x23003F before) whose
 * output buffer (Irp->UserBuffer, METHOD_NEITHER) receives dxgkrnl's
 * DpiInitialize.  DxgkInitialize then calls
 *     DpiInitialize(DriverObject, RegistryPath, DRIVER_INITIALIZATION_DATA *).
 *
 * nvddi attaches an unnamed filter device on top of \Device\DxgKrnl
 * (IoAttachDevice), so IoGetDeviceObjectPointer routes that IRP through us.
 * We forward it, and on success replace the returned pointer with
 * nvddi_dpi_initialize, which hands dxgkrnl a copy of the miniport's
 * DRIVER_INITIALIZATION_DATA whose DxgkDdi* pointers are our wrappers.
 * DXGKRNL_INTERFACE (StartDevice) and the VidPN/monitor interfaces are
 * wrapped the same way (wrap_table).  Only data is changed: no code patching.
 */
#include "nvddi.h"

#define IOCTL_VIDEO_DDI_FUNC_REGISTER       0x0023003F   /* CTL_CODE(VIDEO, 0xF, NEITHER, ANY) */
#define IOCTL_VIDEO_DDI_FUNC_REGISTER_WIN8  0x00230047   /* CTL_CODE(VIDEO, 0x11, NEITHER, ANY) */

typedef NTSTATUS (*DPI_INITIALIZE)(PDRIVER_OBJECT, PUNICODE_STRING, PDRIVER_INITIALIZATION_DATA);

/* wrapped-table cache for static (dxgkrnl-owned) tables */
#define TCACHE 8
static void *s_cache_src[NVDDI_NTABLES][TCACHE];
static void *s_cache_copy[NVDDI_NTABLES][TCACHE];

static void log_irp(PIRP irp, PIO_STACK_LOCATION sl, NTSTATUS st)
{
    NDL_REC *rec;
    NDL_IRP *d;
    if (!g.level)
        return;
    rec = ring_reserve(sizeof(NDL_REC) + sizeof(NDL_IRP), NDL_T_IRP);
    if (!rec)
        return;
    d = (NDL_IRP *)(rec + 1);
    RtlZeroMemory(d, sizeof(*d));
    d->major = sl->MajorFunction;
    d->minor = sl->MinorFunction;
    d->requestor_mode = irp->RequestorMode;
    if (sl->MajorFunction == IRP_MJ_DEVICE_CONTROL || sl->MajorFunction == IRP_MJ_INTERNAL_DEVICE_CONTROL) {
        d->ioctl = sl->Parameters.DeviceIoControl.IoControlCode;
        d->in_len = sl->Parameters.DeviceIoControl.InputBufferLength;
        d->out_len = sl->Parameters.DeviceIoControl.OutputBufferLength;
    }
    d->status = (ULONG)st;
    d->file_object = (ULONG64)(ULONG_PTR)sl->FileObject;
    d->user_buffer = (ULONG64)(ULONG_PTR)irp->UserBuffer;
    ring_commit(rec);
}

static NTSTATUS sync_complete(PDEVICE_OBJECT dev, PIRP irp, PVOID ctx)
{
    UNREFERENCED_PARAMETER(dev);
    UNREFERENCED_PARAMETER(irp);
    KeSetEvent((PKEVENT)ctx, IO_NO_INCREMENT, FALSE);
    return STATUS_MORE_PROCESSING_REQUIRED;
}

static BOOLEAN in_image(PDRIVER_OBJECT d, void *p)
{
    ULONG_PTR a = (ULONG_PTR)p, b;
    if (!d || !d->DriverStart)
        return FALSE;
    b = (ULONG_PTR)d->DriverStart;
    return a >= b && a < b + d->DriverSize;
}

static BOOLEAN is_target(PDRIVER_OBJECT drv)
{
    /* DriverName is "\Driver\<service>"; compare the last component */
    const WCHAR *n;
    USHORT len, i, start = 0;
    WCHAR tmp[64];
    if (!drv || !drv->DriverName.Buffer)
        return FALSE;
    n = drv->DriverName.Buffer;
    len = drv->DriverName.Length / sizeof(WCHAR);
    for (i = 0; i < len; i++)
        if (n[i] == L'\\')
            start = i + 1;
    if (len - start >= 63)
        return FALSE;
    for (i = start; i < len; i++)
        tmp[i - start] = n[i];
    tmp[len - start] = 0;
    if (g.target[0] == L'*' && g.target[1] == 0)
        return TRUE;
    return str_ieq_w(tmp, g.target);
}

static void log_table(ULONG tid, void *src, void *copy, ULONG size, ULONG wrapped, ULONG version)
{
    NDL_REC *rec;
    NDL_TABLE *t;
    NDL_BLOB *b;
    USHORT flags = 0;
    ULONG len = size > 8192 ? 8192 : size;
    rec = ring_reserve(sizeof(NDL_REC) + sizeof(NDL_TABLE) + sizeof(NDL_BLOB) + ALIGN8(len), NDL_T_TABLE);
    if (!rec)
        return;
    t = (NDL_TABLE *)(rec + 1);
    t->table = tid;
    t->wrapped = wrapped;
    t->source = (ULONG64)(ULONG_PTR)src;
    t->copy = (ULONG64)(ULONG_PTR)copy;
    t->size = size;
    t->version = version;
    b = (NDL_BLOB *)(t + 1);
    safe_copy(b + 1, (ULONG_PTR)src, len, KeGetCurrentIrql(), FALSE, &flags);
    if (ALIGN8(len) > len)
        RtlZeroMemory((UCHAR *)(b + 1) + len, ALIGN8(len) - len);
    b->len = len;
    b->orig_len = size;
    b->kind = NDL_B_TABLE;
    b->arg = 0;
    b->rule = 0xFFFF;
    b->flags = flags;
    b->elem = 0xFFFFFFFFu;
    b->parent_rule = 0xFFFF;
    b->depth = 0;
    b->addr = (ULONG64)(ULONG_PTR)src;
    ring_commit(rec);
}

/*
 * Return a copy of the function table 'src' whose known slots point at our
 * wrappers, or src itself when it cannot be wrapped safely.  <= APC_LEVEL.
 */
void *wrap_table(ULONG tid, void *src)
{
    const NVDDI_TABLE *t;
    ULONG size, i, version = 0, wrapped = 0;
    UCHAR *copy;
    KIRQL old;
    USHORT f = 0;
    BOOLEAN mismatch = FALSE;

    if (tid >= NVDDI_NTABLES || !src || KeGetCurrentIrql() > APC_LEVEL)
        return src;
    t = &g_nvddi_tables[tid];
    if (tid == NVDDI_T_DXGKRNL_INTERFACE && !g.hook_callbacks)
        return src;
    if (tid > NVDDI_T_DXGKRNL_INTERFACE && !g.hook_vidpn)
        return src;

    if (t->how == TH_STATIC) {
        for (i = 0; i < TCACHE; i++)
            if (s_cache_src[tid][i] == src)
                return s_cache_copy[tid][i];
    }
    size = t->size;
    if (t->how == TH_SIZE) {
        ULONG sz = 0;
        if (safe_copy(&sz, (ULONG_PTR)src, sizeof(sz), KeGetCurrentIrql(), FALSE, &f) != sizeof(sz) ||
            sz < 16 || sz > 8192) {
            log_event(NDL_EV_TABLE_SKIP, 0, tid, sz, "%s: bad Size field", t->name);
            return src;
        }
        size = sz;   /* copy all the caller has; wrap only the slots we know that fit */
    }
    copy = (UCHAR *)ExAllocatePoolWithTag(NonPagedPoolNx, size, NVDDI_TAG);
    if (!copy)
        return src;
    f = 0;
    if (safe_copy(copy, (ULONG_PTR)src, size, KeGetCurrentIrql(), FALSE, &f) != size) {
        ExFreePoolWithTag(copy, NVDDI_TAG);
        log_event(NDL_EV_TABLE_SKIP, 0, tid, (ULONG_PTR)src, "%s: unreadable", t->name);
        return src;
    }
    if (t->maxver) {
        version = *(ULONG *)copy;
        if (version > t->maxver) {
            ExFreePoolWithTag(copy, NVDDI_TAG);
            log_event(NDL_EV_TABLE_SKIP, 0, tid, version, "%s: version %u newer than the headers", t->name,
                      version);
            return src;
        }
    }

    KeAcquireSpinLock(&g.table_lock, &old);
    /* every slot must agree with what earlier copies of this table type had */
    for (i = 0; i < t->nslots; i++) {
        const NVDDI_SLOT *s = &t->slots[i];
        void *fn;
        if ((ULONG)s->field_off + sizeof(void *) > size)
            continue;
        fn = *(void **)(copy + s->field_off);
        if (fn && t->orig[i] && t->orig[i] != fn)
            mismatch = TRUE;
    }
    if (!mismatch) {
        for (i = 0; i < t->nslots; i++) {
            const NVDDI_SLOT *s = &t->slots[i];
            void *fn;
            if ((ULONG)s->field_off + sizeof(void *) > size)
                continue;
            fn = *(void **)(copy + s->field_off);
            if (!fn)
                continue;
            t->orig[i] = fn;
            *(void **)(copy + s->field_off) = s->wrapper;
            wrapped++;
        }
        if (t->how == TH_STATIC) {
            for (i = 0; i < TCACHE; i++) {
                if (!s_cache_src[tid][i]) {
                    s_cache_src[tid][i] = src;
                    s_cache_copy[tid][i] = copy;
                    break;
                }
            }
        }
    }
    KeReleaseSpinLock(&g.table_lock, old);

    if (mismatch) {
        ExFreePoolWithTag(copy, NVDDI_TAG);
        log_event(NDL_EV_TABLE_SKIP, 0, tid, (ULONG_PTR)src, "%s: differs from the table wrapped before",
                  t->name);
        return src;
    }
    log_table(tid, src, copy, size, wrapped, version);
    return copy;
}

static void log_register(PDRIVER_OBJECT drv, PUNICODE_STRING reg, PDRIVER_INITIALIZATION_DATA init,
                         ULONG hooked, NTSTATUS st, ULONG init_len)
{
    NDL_REC *rec;
    NDL_REGISTER *r;
    USHORT nl = 0, rl = 0;
    ULONG size, strl;
    UCHAR *p;
    NDL_BLOB *b;
    USHORT f = 0;
    if (drv && drv->DriverName.Buffer)
        nl = drv->DriverName.Length > 256 ? 256 : drv->DriverName.Length;
    if (reg && reg->Buffer)
        rl = reg->Length > 512 ? 512 : reg->Length;
    strl = ALIGN8((ULONG)nl + rl);
    size = sizeof(NDL_REC) + sizeof(NDL_REGISTER) + strl + (init ? sizeof(NDL_BLOB) + ALIGN8(init_len) : 0);
    rec = ring_reserve(size, NDL_T_REGISTER);
    if (!rec)
        return;
    r = (NDL_REGISTER *)(rec + 1);
    r->driver_object = (ULONG64)(ULONG_PTR)drv;
    r->image_base = drv ? (ULONG64)(ULONG_PTR)drv->DriverStart : 0;
    r->image_size = drv ? drv->DriverSize : 0;
    r->init_version = init ? init->Version : 0;
    r->hooked = hooked;
    r->status = (ULONG)st;
    r->dpi_initialize = (ULONG64)(ULONG_PTR)g.real_dpi;
    r->name_len = nl;
    r->regpath_len = rl;
    r->nblobs = init ? 1 : 0;
    p = (UCHAR *)(r + 1);
    RtlZeroMemory(p, strl);
    if (nl)
        RtlCopyMemory(p, drv->DriverName.Buffer, nl);
    if (rl)
        RtlCopyMemory(p + nl, reg->Buffer, rl);
    p += strl;
    if (init) {
        b = (NDL_BLOB *)p;
        safe_copy(b + 1, (ULONG_PTR)init, init_len, KeGetCurrentIrql(), FALSE, &f);
        if (ALIGN8(init_len) > init_len)
            RtlZeroMemory((UCHAR *)(b + 1) + init_len, ALIGN8(init_len) - init_len);
        b->len = init_len;
        b->orig_len = init_len;
        b->kind = NDL_B_INIT;
        b->arg = 2;
        b->rule = 0xFFFF;
        b->flags = f;
        b->elem = 0xFFFFFFFFu;
        b->parent_rule = 0xFFFF;
        b->depth = 0;
        b->addr = (ULONG64)(ULONG_PTR)init;
    }
    ring_commit(rec);
}

/* Extra bytes copied past our DRIVER_INITIALIZATION_DATA for newer miniports. */
#define INIT_TAIL 0x800

static NTSTATUS nvddi_dpi_initialize(PDRIVER_OBJECT drv, PUNICODE_STRING reg, PDRIVER_INITIALIZATION_DATA init)
{
    DPI_INITIALIZE real = (DPI_INITIALIZE)g.real_dpi;
    const NVDDI_TABLE *t = &g_nvddi_tables[NVDDI_T_DRIVER_INITIALIZATION_DATA];
    UCHAR *copy = NULL;
    ULONG len = sizeof(DRIVER_INITIALIZATION_DATA), i, wrapped = 0;
    NTSTATUS st;
    USHORT f = 0;
    const char *why = NULL;

    InterlockedIncrement(&g.registrations);
    if (!real)
        return STATUS_UNSUCCESSFUL;
    if (!init)
        why = "no init data";
    else if (!g.enabled || g.fail_count >= g.max_fails)
        why = "disabled";
    else if (!is_target(drv))
        why = "not the target driver";
    else if (KeGetCurrentIrql() != PASSIVE_LEVEL)
        why = "irql";
    if (!why) {
        if (init->Version > DXGKDDI_INTERFACE_VERSION)
            len += INIT_TAIL;   /* newer headers than ours: carry fields we do not know */
        copy = (UCHAR *)ExAllocatePoolWithTag(NonPagedPoolNx, len, NVDDI_TAG);
        if (!copy)
            why = "no memory";
    }
    if (why) {
        log_register(drv, reg, init, 0, 0, init ? sizeof(DRIVER_INITIALIZATION_DATA) : 0);
        log_event(NDL_EV_HOOK_SKIP, 0, (ULONG_PTR)drv, init ? init->Version : 0, "%ls: %s",
                  drv && drv->DriverName.Buffer ? drv->DriverName.Buffer : L"?", why);
        return real(drv, reg, init);
    }
    /* a partial read past the end of a shorter structure leaves zeros (= not implemented) */
    safe_copy(copy, (ULONG_PTR)init, len, PASSIVE_LEVEL, FALSE, &f);
    for (i = 0; i < t->nslots; i++) {
        const NVDDI_SLOT *s = &t->slots[i];
        void *fn = *(void **)(copy + s->field_off);
        if (!fn)
            continue;
        t->orig[i] = fn;
        *(void **)(copy + s->field_off) = s->wrapper;
        wrapped++;
    }
    g.hooked_drv = drv;
    g.tgt_size = drv->DriverSize;
    InterlockedExchangePointer((PVOID volatile *)&g.tgt_base, drv->DriverStart);
    {
        ULONG k, start = 0, n = drv->DriverName.Length / sizeof(WCHAR);
        for (k = 0; k < n; k++)
            if (drv->DriverName.Buffer[k] == L'\\')
                start = k + 1;
        for (k = start; k < n && k - start < sizeof(g.hooked_name) - 1; k++)
            g.hooked_name[k - start] = (char)drv->DriverName.Buffer[k];
        g.hooked_name[k - start] = 0;
    }
    log_register(drv, reg, init, 1, 0, sizeof(DRIVER_INITIALIZATION_DATA));
    log_table(NVDDI_T_DRIVER_INITIALIZATION_DATA, init, copy, sizeof(DRIVER_INITIALIZATION_DATA), wrapped,
              init->Version);
    st = real(drv, reg, (PDRIVER_INITIALIZATION_DATA)copy);
    if (NT_SUCCESS(st))
        InterlockedIncrement(&g.hooked);
    log_event(NDL_EV_INFO, st, (ULONG_PTR)drv->DriverStart, drv->DriverSize,
              "DpiInitialize(%s) with %u wrapped DDIs, version 0x%x", g.hooked_name, wrapped, init->Version);
    /* the copy stays allocated: dxgkrnl may keep referring to it */
    return st;
}

static NTSTATUS handle_register(PDEVICE_OBJECT dev, PIRP irp, PIO_STACK_LOCATION sl)
{
    KEVENT ev;
    NTSTATUS st;
    ULONG code = sl->Parameters.DeviceIoControl.IoControlCode;
    UNREFERENCED_PARAMETER(dev);

    KeInitializeEvent(&ev, NotificationEvent, FALSE);
    IoCopyCurrentIrpStackLocationToNext(irp);
    IoSetCompletionRoutine(irp, sync_complete, &ev, TRUE, TRUE, TRUE);
    st = IoCallDriver(g.lower, irp);
    if (st == STATUS_PENDING) {
        KeWaitForSingleObject(&ev, Executive, KernelMode, FALSE, NULL);
        st = irp->IoStatus.Status;
    }
    if (NT_SUCCESS(st) && irp->RequestorMode == KernelMode && irp->UserBuffer &&
        sl->Parameters.DeviceIoControl.OutputBufferLength >= sizeof(void *) && is_kernel_address((ULONG_PTR)irp->UserBuffer)) {
        void *fn = *(void **)irp->UserBuffer;
        if (fn && in_image(g.dxgkrnl, fn) && (!g.real_dpi || g.real_dpi == fn)) {
            g.real_dpi = fn;
            *(void **)irp->UserBuffer = (void *)nvddi_dpi_initialize;
            log_event(NDL_EV_REGISTER_IOCTL, st, (ULONG_PTR)fn, code, "registration IOCTL 0x%x: DpiInitialize %p",
                      code, fn);
        } else {
            log_event(NDL_EV_REGISTER_IOCTL, st, (ULONG_PTR)fn, code,
                      "registration IOCTL 0x%x: unexpected function %p, passed through", code, fn);
        }
    }
    log_irp(irp, sl, st);
    IoCompleteRequest(irp, IO_NO_INCREMENT);
    return st;
}

NTSTATUS filter_dispatch(PDEVICE_OBJECT dev, PIRP irp)
{
    PIO_STACK_LOCATION sl = IoGetCurrentIrpStackLocation(irp);
    if (sl->MajorFunction == IRP_MJ_INTERNAL_DEVICE_CONTROL && KeGetCurrentIrql() == PASSIVE_LEVEL &&
        g.enabled && (sl->Parameters.DeviceIoControl.IoControlCode == IOCTL_VIDEO_DDI_FUNC_REGISTER_WIN8 ||
                      sl->Parameters.DeviceIoControl.IoControlCode == IOCTL_VIDEO_DDI_FUNC_REGISTER))
        return handle_register(dev, irp, sl);
    log_irp(irp, sl, STATUS_PENDING);
    IoSkipCurrentIrpStackLocation(irp);
    return IoCallDriver(g.lower, irp);
}

static NTSTATUS load_dxgkrnl(void)
{
    UNICODE_STRING svc;
    RtlInitUnicodeString(&svc, L"\\Registry\\Machine\\System\\CurrentControlSet\\Services\\DXGKrnl");
    return ZwLoadDriver(&svc);
}

NTSTATUS filter_attach(void)
{
    UNICODE_STRING name;
    NTSTATUS st;
    PDEVICE_OBJECT fdo, base;
    int tries;

    st = IoCreateDevice(g.driver, 0, NULL, FILE_DEVICE_VIDEO, FILE_DEVICE_SECURE_OPEN, FALSE, &fdo);
    if (!NT_SUCCESS(st))
        return st;
    RtlInitUnicodeString(&name, L"\\Device\\DxgKrnl");
    st = IoAttachDevice(fdo, &name, &g.lower);
    if (st == STATUS_OBJECT_NAME_NOT_FOUND || st == STATUS_OBJECT_PATH_NOT_FOUND || st == STATUS_NO_SUCH_DEVICE) {
        /* same as displib: load the DXGKrnl service, then retry for up to 50 ms */
        NTSTATUS ls = load_dxgkrnl();
        log_event(NDL_EV_INFO, ls, 0, 0, "ZwLoadDriver(DXGKrnl)");
        for (tries = 0; tries < 10; tries++) {
            LARGE_INTEGER d;
            st = IoAttachDevice(fdo, &name, &g.lower);
            if (NT_SUCCESS(st))
                break;
            d.QuadPart = -50000;
            KeDelayExecutionThread(KernelMode, FALSE, &d);
        }
    }
    if (!NT_SUCCESS(st)) {
        IoDeleteDevice(fdo);
        log_event(NDL_EV_ATTACH_FAIL, st, 0, 0, "IoAttachDevice(\\Device\\DxgKrnl) failed");
        return st;
    }
    fdo->Flags |= g.lower->Flags & (DO_BUFFERED_IO | DO_DIRECT_IO | DO_POWER_PAGABLE);
    fdo->Characteristics |= g.lower->Characteristics & ~FILE_DEVICE_SECURE_OPEN;
    fdo->DeviceType = g.lower->DeviceType;
    fdo->Flags &= ~DO_DEVICE_INITIALIZING;
    g.filter = fdo;
    base = IoGetDeviceAttachmentBaseRef(g.lower);
    if (base) {
        g.dxgkrnl = base->DriverObject;
        ObDereferenceObject(base);
    }
    InterlockedExchange(&g.attached, 1);
    log_event(NDL_EV_ATTACH, 0, (ULONG_PTR)g.lower, (ULONG_PTR)(g.dxgkrnl ? g.dxgkrnl->DriverStart : 0),
              "attached to \\Device\\DxgKrnl (dxgkrnl image %p size 0x%x)",
              g.dxgkrnl ? g.dxgkrnl->DriverStart : NULL, g.dxgkrnl ? g.dxgkrnl->DriverSize : 0);
    return STATUS_SUCCESS;
}
