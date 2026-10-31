/*
 * nvddi.sys entry: configuration, self-disable counter, control device.
 *
 * Registry (HKLM\SYSTEM\CurrentControlSet\Services\nvddi\Parameters), DWORDs
 * unless noted:
 *   Enabled        1     0: load but do not attach or hook
 *   Level          3     0 off, 1 calls, 2 + structs, 3 + nested buffers
 *   Target         "nvlddmkm" (REG_SZ) service name of the miniport to hook, "*" = first
 *   LogDir         "\??\C:\nvddi" (REG_SZ, NT path)
 *   RingKB         8192  per-CPU ring (power of two, >= 256)
 *   TotalRingMB    256   cap for all rings together (nonpaged)
 *   MaxBlobKB      2048  cap for one captured buffer (larger ones are cut and flagged TRUNC;
 *                        effective cap is a quarter of a ring)
 *   MaxFileMB      4096  stop writing a file at this size
 *   HookCallbacks  1     wrap DXGKRNL_INTERFACE (DxgkCb*)
 *   HookVidPn      1     wrap VidPN / monitor interfaces
 *   LogRegistry    1     record registry accesses made from the hooked miniport's code
 *   ResolveGpuVa   1     reconstruct GpuMmu mappings and copy command bytes at submit
 *   GpuVaEntries   0     VA map entries (0 = 1M = 32 MB nonpaged); power of two
 *   HookImports    0     wrap nvlddmkm's ntoskrnl/hal imports via IAT (HVCI must be OFF)
 *   BootStart      0     driver was set to boot/system start to wrap the first bring-up;
 *                        retry attaching to \Device\DxgKrnl via driver reinit if not ready
 *   Mute           (REG_MULTI_SZ) slot names not to record
 *   MaxFails       2     load attempts that may end without "healthy"
 *   FailCount      -     incremented at every load, cleared after
 *   HealthySeconds 90    seconds of uptime; at MaxFails nvddi does nothing
 *   BootCount      -     incremented at every load (file names)
 */
#include "nvddi.h"

NVDDI_GLOBALS g;

static WCHAR s_regpath[256];

/* ------------------------------------------------------------ registry */

static NTSTATUS open_params(HANDLE *h, BOOLEAN create)
{
    WCHAR path[300];
    UNICODE_STRING u;
    OBJECT_ATTRIBUTES oa;
    ULONG i, n = 0;
    const WCHAR *sfx = L"\\Parameters";
    for (i = 0; s_regpath[i] && n < 250; i++)
        path[n++] = s_regpath[i];
    for (i = 0; sfx[i]; i++)
        path[n++] = sfx[i];
    path[n] = 0;
    RtlInitUnicodeString(&u, path);
    InitializeObjectAttributes(&oa, &u, OBJ_KERNEL_HANDLE | OBJ_CASE_INSENSITIVE, NULL, NULL);
    if (create)
        return ZwCreateKey(h, KEY_READ | KEY_WRITE, &oa, 0, NULL, REG_OPTION_NON_VOLATILE, NULL);
    return ZwOpenKey(h, KEY_READ, &oa);
}

static ULONG cfg_dword(HANDLE h, const WCHAR *name, ULONG def)
{
    UCHAR buf[sizeof(KEY_VALUE_PARTIAL_INFORMATION) + 8];
    PKEY_VALUE_PARTIAL_INFORMATION v = (PKEY_VALUE_PARTIAL_INFORMATION)buf;
    UNICODE_STRING u;
    ULONG len;
    if (!h)
        return def;
    RtlInitUnicodeString(&u, name);
    if (!NT_SUCCESS(ZwQueryValueKey(h, &u, KeyValuePartialInformation, v, sizeof(buf), &len)) ||
        v->Type != REG_DWORD || v->DataLength != 4)
        return def;
    return *(ULONG *)v->Data;
}

static void cfg_string(HANDLE h, const WCHAR *name, WCHAR *out, ULONG cch, const WCHAR *def)
{
    UCHAR buf[sizeof(KEY_VALUE_PARTIAL_INFORMATION) + 512];
    PKEY_VALUE_PARTIAL_INFORMATION v = (PKEY_VALUE_PARTIAL_INFORMATION)buf;
    UNICODE_STRING u;
    ULONG len, i, n;
    const WCHAR *src = def;
    n = 0xFFFFFFFF;
    if (h) {
        RtlInitUnicodeString(&u, name);
        if (NT_SUCCESS(ZwQueryValueKey(h, &u, KeyValuePartialInformation, v, sizeof(buf), &len)) &&
            (v->Type == REG_SZ || v->Type == REG_EXPAND_SZ) && v->DataLength >= 2) {
            src = (const WCHAR *)v->Data;
            n = v->DataLength / sizeof(WCHAR);
        }
    }
    for (i = 0; i + 1 < cch && i < n && src[i]; i++)
        out[i] = src[i];
    out[i] = 0;
}

static void cfg_mute(HANDLE h)
{
    UCHAR *buf;
    PKEY_VALUE_PARTIAL_INFORMATION v;
    UNICODE_STRING u;
    ULONG len, i;
    const ULONG cap = 8192;
    if (!h)
        return;
    buf = (UCHAR *)ExAllocatePoolWithTag(PagedPool, cap, NVDDI_TAG);
    if (!buf)
        return;
    v = (PKEY_VALUE_PARTIAL_INFORMATION)buf;
    RtlInitUnicodeString(&u, L"Mute");
    if (NT_SUCCESS(ZwQueryValueKey(h, &u, KeyValuePartialInformation, v, cap, &len)) &&
        (v->Type == REG_MULTI_SZ || v->Type == REG_SZ)) {
        const WCHAR *w = (const WCHAR *)v->Data;
        ULONG n = v->DataLength / sizeof(WCHAR);
        char name[128];
        ULONG k = 0;
        for (i = 0; i <= n; i++) {
            WCHAR c = i < n ? w[i] : 0;
            if (c == 0 || c == L',' || c == L' ' || c == L';') {
                if (k) {
                    name[k] = 0;
                    mute_by_name(name, 1);
                }
                k = 0;
            } else if (k < sizeof(name) - 1) {
                name[k++] = (char)c;
            }
        }
    }
    ExFreePoolWithTag(buf, NVDDI_TAG);
}

NTSTATUS cfg_write_dword(const WCHAR *name, ULONG val)
{
    HANDLE h;
    UNICODE_STRING u;
    NTSTATUS st;
    if (KeGetCurrentIrql() != PASSIVE_LEVEL)
        return STATUS_INVALID_DEVICE_STATE;
    st = open_params(&h, TRUE);
    if (!NT_SUCCESS(st))
        return st;
    RtlInitUnicodeString(&u, name);
    st = ZwSetValueKey(h, &u, 0, REG_DWORD, &val, sizeof(val));
    ZwFlushKey(h);
    ZwClose(h);
    return st;
}

void nvddi_set_fail_count(ULONG v)
{
    cfg_write_dword(L"FailCount", v);
}

/* ------------------------------------------------------------ mute */

static int str_eq(const char *a, const char *b)
{
    while (*a && *a == *b) {
        a++;
        b++;
    }
    return *a == *b;
}

ULONG mute_by_name(const char *name, ULONG mute)
{
    ULONG t, i, hits = 0;
    BOOLEAN all = name[0] == '*' && name[1] == 0;
    for (t = 0; t < NVDDI_NTABLES; t++) {
        const NVDDI_TABLE *tb = &g_nvddi_tables[t];
        for (i = 0; i < tb->nslots && i < 256; i++) {
            if (all || str_eq(tb->slots[i].name, name)) {
                if (mute)
                    InterlockedOr(&g.mute[t][i >> 5], (LONG)(1u << (i & 31)));
                else
                    InterlockedAnd(&g.mute[t][i >> 5], (LONG) ~(1u << (i & 31)));
                hits++;
            }
        }
    }
    return hits;
}

/* ------------------------------------------------------------ control device */

static NTSTATUS complete(PIRP irp, NTSTATUS st, ULONG_PTR info)
{
    irp->IoStatus.Status = st;
    irp->IoStatus.Information = info;
    IoCompleteRequest(irp, IO_NO_INCREMENT);
    return st;
}

static BOOLEAN caller_is_admin(void)
{
    SECURITY_SUBJECT_CONTEXT sc;
    BOOLEAN admin;
    SeCaptureSubjectContext(&sc);
    SeLockSubjectContext(&sc);
    admin = SeTokenIsAdmin(SeQuerySubjectContextToken(&sc));
    SeUnlockSubjectContext(&sc);
    SeReleaseSubjectContext(&sc);
    return admin;
}

static NTSTATUS ctl_ioctl(PIRP irp, PIO_STACK_LOCATION sl)
{
    ULONG code = sl->Parameters.DeviceIoControl.IoControlCode;
    ULONG inl = sl->Parameters.DeviceIoControl.InputBufferLength;
    ULONG outl = sl->Parameters.DeviceIoControl.OutputBufferLength;
    void *buf = irp->AssociatedIrp.SystemBuffer;

    switch (code) {
    case IOCTL_NVDDI_STATUS: {
        NVDDI_STATUS *s = (NVDDI_STATUS *)buf;
        ULONG i;
        LONG64 dropped = 0;
        if (outl < sizeof(NVDDI_STATUS))
            return complete(irp, STATUS_BUFFER_TOO_SMALL, 0);
        RtlZeroMemory(s, sizeof(*s));
        s->version = NVDDI_VERSION;
        s->level = (ULONG)g.level;
        s->capturing = g.state == ST_CAPTURE;
        s->attached = (ULONG)g.attached;
        s->registrations = (ULONG)g.registrations;
        s->hooked = (ULONG)g.hooked;
        s->ncpu = g.ncpu;
        s->ring_bytes = g.ring_bytes;
        s->seq = (ULONG64)g.seq;
        for (i = 0; i < g.ncpu; i++)
            dropped += g.rings[i].dropped;
        s->dropped = (ULONG64)dropped;
        s->dropped_file = (ULONG64)g.dropped_file;
        s->file_bytes = (ULONG64)g.file_bytes;
        s->calls = (ULONG64)g.calls;
        s->fail_count = g.fail_count;
        s->last_error = (ULONG)g.last_error;
        for (i = 0; i < 199 && g.file_name[i]; i++)
            s->file_name[i] = g.file_name[i];
        for (i = 0; i < 63 && g.hooked_name[i]; i++)
            s->hooked_driver[i] = g.hooked_name[i];
        return complete(irp, STATUS_SUCCESS, sizeof(NVDDI_STATUS));
    }
    case IOCTL_NVDDI_START:
        if (!g.level)
            InterlockedExchange(&g.level, g.cfg_level ? (LONG)g.cfg_level : NVDDI_LEVEL_FULL);
        writer_command(WCMD_START, 5000);
        return complete(irp, STATUS_SUCCESS, 0);
    case IOCTL_NVDDI_STOP:
        writer_command(WCMD_STOP, 10000);
        return complete(irp, STATUS_SUCCESS, 0);
    case IOCTL_NVDDI_FLUSH:
        writer_command(WCMD_FLUSH, 10000);
        return complete(irp, STATUS_SUCCESS, 0);
    case IOCTL_NVDDI_SET_LEVEL:
        if (inl < sizeof(ULONG) || *(ULONG *)buf > NVDDI_LEVEL_FULL)
            return complete(irp, STATUS_INVALID_PARAMETER, 0);
        InterlockedExchange(&g.level, (LONG) * (ULONG *)buf);
        log_event(NDL_EV_LEVEL, 0, *(ULONG *)buf, 0, "level %u", *(ULONG *)buf);
        return complete(irp, STATUS_SUCCESS, 0);
    case IOCTL_NVDDI_MUTE: {
        NVDDI_MUTE *m = (NVDDI_MUTE *)buf;
        ULONG hits;
        if (inl < sizeof(NVDDI_MUTE))
            return complete(irp, STATUS_INVALID_PARAMETER, 0);
        m->name[sizeof(m->name) - 1] = 0;
        hits = mute_by_name(m->name, m->mute);
        log_event(NDL_EV_INFO, 0, m->mute, hits, "%s %s (%u slots)", m->mute ? "mute" : "unmute", m->name, hits);
        return complete(irp, hits ? STATUS_SUCCESS : STATUS_NOT_FOUND, 0);
    }
    case IOCTL_NVDDI_MARK: {
        char text[200];
        ULONG n = inl < sizeof(text) - 1 ? inl : sizeof(text) - 1;
        if (n)
            RtlCopyMemory(text, buf, n);
        text[n] = 0;
        log_event(NDL_EV_INFO, 0, 0, 0, "mark: %s", text);
        return complete(irp, STATUS_SUCCESS, 0);
    }
    default:
        return complete(irp, STATUS_INVALID_DEVICE_REQUEST, 0);
    }
}

static NTSTATUS dispatch(PDEVICE_OBJECT dev, PIRP irp)
{
    PIO_STACK_LOCATION sl = IoGetCurrentIrpStackLocation(irp);
    if (dev != g.ctl)
        return filter_dispatch(dev, irp);
    switch (sl->MajorFunction) {
    case IRP_MJ_CREATE:
        if (irp->RequestorMode == UserMode && !caller_is_admin())
            return complete(irp, STATUS_ACCESS_DENIED, 0);
        return complete(irp, STATUS_SUCCESS, 0);
    case IRP_MJ_CLOSE:
    case IRP_MJ_CLEANUP:
        return complete(irp, STATUS_SUCCESS, 0);
    case IRP_MJ_DEVICE_CONTROL:
        return ctl_ioctl(irp, sl);
    default:
        return complete(irp, STATUS_INVALID_DEVICE_REQUEST, 0);
    }
}

static NTSTATUS create_ctl(void)
{
    UNICODE_STRING dn, ln;
    NTSTATUS st;
    RtlInitUnicodeString(&dn, NVDDI_CTL_DEVICE);
    RtlInitUnicodeString(&ln, NVDDI_CTL_LINK);
    st = IoCreateDevice(g.driver, 0, &dn, FILE_DEVICE_UNKNOWN, FILE_DEVICE_SECURE_OPEN, FALSE, &g.ctl);
    if (!NT_SUCCESS(st))
        return st;
    st = IoCreateSymbolicLink(&ln, &dn);
    if (!NT_SUCCESS(st)) {
        IoDeleteDevice(g.ctl);
        g.ctl = NULL;
        return st;
    }
    g.ctl->Flags |= DO_BUFFERED_IO;
    g.ctl->Flags &= ~DO_DEVICE_INITIALIZING;
    return STATUS_SUCCESS;
}

static ULONG pow2_floor(ULONG v)
{
    ULONG p = 1;
    while (p <= v / 2)
        p <<= 1;
    return p;
}

NTSTATUS DriverEntry(PDRIVER_OBJECT drv, PUNICODE_STRING regpath)
{
    HANDLE h = NULL;
    NTSTATUS st;
    ULONG i, ncpu, ring;

    RtlZeroMemory(&g, sizeof(g));
    g.driver = drv;
    g.load_qpc = (ULONG64)KeQueryPerformanceCounter(&g.qpc_freq).QuadPart;
    KeInitializeSpinLock(&g.table_lock);
    for (i = 0; i < regpath->Length / sizeof(WCHAR) && i < 255; i++)
        s_regpath[i] = regpath->Buffer[i];
    s_regpath[i] = 0;

    /* never in safe mode: the recovery path must not depend on this driver */
    if (InitSafeBootMode)
        return STATUS_NOT_SUPPORTED;

    open_params(&h, TRUE);
    g.enabled = cfg_dword(h, L"Enabled", 1);
    g.cfg_level = cfg_dword(h, L"Level", NVDDI_LEVEL_FULL);
    g.ring_kb = cfg_dword(h, L"RingKB", 8192);
    g.total_ring_mb = cfg_dword(h, L"TotalRingMB", 256);
    g.max_blob = cfg_dword(h, L"MaxBlobKB", 2048) * 1024;
    g.max_file_mb = cfg_dword(h, L"MaxFileMB", 4096);
    g.hook_callbacks = cfg_dword(h, L"HookCallbacks", 1);
    g.hook_vidpn = cfg_dword(h, L"HookVidPn", 1);
    g.log_registry = cfg_dword(h, L"LogRegistry", 1);
    g.resolve_gpuva = cfg_dword(h, L"ResolveGpuVa", 1);
    g.gpuva_entries = cfg_dword(h, L"GpuVaEntries", 0);
    g.hook_imports = cfg_dword(h, L"HookImports", 0);
    g.boot_start = cfg_dword(h, L"BootStart", 0);
    g.max_fails = cfg_dword(h, L"MaxFails", 2);
    g.fail_count = cfg_dword(h, L"FailCount", 0);
    g.boot_count = cfg_dword(h, L"BootCount", 0) + 1;
    g.healthy_seconds = cfg_dword(h, L"HealthySeconds", 90);
    cfg_string(h, L"LogDir", g.logdir, 128, L"\\??\\C:\\nvddi");
    cfg_string(h, L"Target", g.target, 64, L"nvlddmkm");
    if (h)
        ZwClose(h);
    if (g.cfg_level > NVDDI_LEVEL_FULL)
        g.cfg_level = NVDDI_LEVEL_FULL;
    if (g.max_blob < 4096)
        g.max_blob = 4096;
    if (g.max_fails == 0)
        g.max_fails = 2;
    cfg_write_dword(L"BootCount", g.boot_count);

    if (!g.enabled)
        return STATUS_NOT_SUPPORTED;
    if (g.fail_count >= g.max_fails) {
        /* two loads in a row never became healthy: stay out of the way */
        return STATUS_NOT_SUPPORTED;
    }
    cfg_write_dword(L"FailCount", g.fail_count + 1);

    ncpu = KeQueryActiveProcessorCountEx(ALL_PROCESSOR_GROUPS);
    if (!ncpu)
        ncpu = 1;
    ring = g.ring_kb * 1024;
    if (ring > (g.total_ring_mb * 1024 * 1024) / ncpu)
        ring = (g.total_ring_mb * 1024 * 1024) / ncpu;
    if (ring < 256 * 1024)
        ring = 256 * 1024;
    ring = pow2_floor(ring);
    if (g.max_blob > ring / 4)
        g.max_blob = ring / 4;
    st = ring_init(ring);
    if (!NT_SUCCESS(st))
        return st;
    if (g.resolve_gpuva && !NT_SUCCESS(gpuva_init())) {
        g.resolve_gpuva = 0;
        InterlockedExchange(&g.last_error, STATUS_INSUFFICIENT_RESOURCES);
    }
    InterlockedExchange(&g.level, (LONG)g.cfg_level);
    InterlockedExchange(&g.state, ST_PENDING);

    for (i = 0; i <= IRP_MJ_MAXIMUM_FUNCTION; i++)
        drv->MajorFunction[i] = dispatch;
    /* no DriverUnload: once attached to \Device\DxgKrnl and handing out
       wrappers, the image must stay loaded until reboot */

    {
        HANDLE hm = NULL;
        if (NT_SUCCESS(open_params(&hm, FALSE))) {
            cfg_mute(hm);
            ZwClose(hm);
        }
    }

    log_event(NDL_EV_LOAD, 0, g.boot_count, g.fail_count,
              "nvddi %u.%u.%u level=%u target=%ls rings=%ux%uKB maxblob=%uKB failcount=%u",
              NVDDI_VERSION >> 24, (NVDDI_VERSION >> 16) & 0xFF, NVDDI_VERSION & 0xFFFF, g.cfg_level, g.target,
              g.ncpu, g.ring_bytes / 1024, g.max_blob / 1024, g.fail_count);

    st = create_ctl();
    if (!NT_SUCCESS(st))
        log_event(NDL_EV_INFO, st, 0, 0, "control device not created");
    st = writer_start();
    if (!NT_SUCCESS(st))
        log_event(NDL_EV_INFO, st, 0, 0, "writer thread not started");
    st = filter_attach();
    if (!NT_SUCCESS(st)) {
        InterlockedExchange(&g.last_error, st);
        /* boot/system start: \Device\DxgKrnl may not be up yet; retry via reinit */
        IoRegisterDriverReinitialization(drv, filter_reinit, NULL);
    }
    st = registry_init();
    if (!NT_SUCCESS(st))
        log_event(NDL_EV_INFO, st, 0, 0, "registry callback not registered");
    /* stay loaded even if attaching failed: the control device reports why */
    return STATUS_SUCCESS;
}
