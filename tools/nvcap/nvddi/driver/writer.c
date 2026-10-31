/*
 * Writer thread (PASSIVE_LEVEL): drains the per-CPU rings into the log file.
 *
 * File: <LogDir>\nvddi-<YYYYMMDD-HHMMSS>-b<boot>-<n>.ndl, header + schema
 * record + records.  Until the first file can be created (early boot, volume
 * not mounted yet) records stay in the rings.  If LogDir cannot be used for
 * 30 s the writer falls back to \SystemRoot\nvddi.  Writing stops at
 * MaxFileMB; later records are counted as dropped.
 */
#include "nvddi.h"

#define STAGE_BYTES (1024 * 1024)

static HANDLE   s_file;
static UCHAR   *s_stage;
static ULONG    s_stage_len;
static ULONG64  s_records;
static BOOLEAN  s_cap_writing;
static BOOLEAN  s_healthy_done;
static LARGE_INTEGER s_last_flush;

static NTSTATUS make_dir(const WCHAR *dir)
{
    UNICODE_STRING u;
    OBJECT_ATTRIBUTES oa;
    IO_STATUS_BLOCK io;
    HANDLE h;
    NTSTATUS st;
    RtlInitUnicodeString(&u, dir);
    InitializeObjectAttributes(&oa, &u, OBJ_KERNEL_HANDLE | OBJ_CASE_INSENSITIVE, NULL, NULL);
    st = ZwCreateFile(&h, FILE_LIST_DIRECTORY | SYNCHRONIZE, &oa, &io, NULL, FILE_ATTRIBUTE_NORMAL,
                      FILE_SHARE_READ | FILE_SHARE_WRITE, FILE_OPEN_IF,
                      FILE_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT, NULL, 0);
    if (NT_SUCCESS(st))
        ZwClose(h);
    return st;
}

static NTSTATUS write_raw(const void *p, ULONG len)
{
    IO_STATUS_BLOCK io;
    NTSTATUS st;
    if (!s_file)
        return STATUS_INVALID_HANDLE;
    st = ZwWriteFile(s_file, NULL, NULL, NULL, &io, (PVOID)p, len, NULL, NULL);
    if (NT_SUCCESS(st))
        InterlockedAdd64(&g.file_bytes, len);
    return st;
}

static void stage_flush(void)
{
    if (s_stage_len) {
        NTSTATUS st = write_raw(s_stage, s_stage_len);
        if (!NT_SUCCESS(st))
            InterlockedExchange(&g.last_error, st);
        s_stage_len = 0;
    }
}

static BOOLEAN file_full(void)
{
    return (ULONG64)g.file_bytes + s_stage_len >= (ULONG64)g.max_file_mb * 1024 * 1024;
}

static BOOLEAN s_cap_marked;
static void put_own(USHORT type, const void *payload, ULONG plen);

static void stage_put(const void *p, ULONG len)
{
    if (s_file && file_full() && !s_cap_marked) {
        /* one marker record past the cap so the cut is visible in the file */
        NDL_DROP dr = {0, 0, 0, 2};
        s_cap_marked = TRUE;
        put_own(NDL_T_DROP, &dr, sizeof(dr));
    }
    if (!s_file || (file_full() && s_cap_marked && !s_cap_writing)) {
        InterlockedIncrement64(&g.dropped_file);
        return;
    }
    if (len > STAGE_BYTES) {
        stage_flush();
        write_raw(p, len);
    } else {
        if (s_stage_len + len > STAGE_BYTES)
            stage_flush();
        RtlCopyMemory(s_stage + s_stage_len, p, len);
        s_stage_len += len;
    }
    s_records++;
}

/* a record built by the writer itself (not from a ring) */
static void put_own(USHORT type, const void *payload, ULONG plen)
{
    UCHAR buf[sizeof(NDL_REC) + 64];
    NDL_REC *r = (NDL_REC *)buf;
    ULONG size = ALIGN8(sizeof(NDL_REC) + plen);
    if (size > sizeof(buf))
        return;
    RtlZeroMemory(buf, sizeof(buf));
    s_cap_writing = TRUE;
    r->size = size;
    r->type = type;
    r->seq = (ULONG64)InterlockedIncrement64(&g.seq);
    r->qpc = (ULONG64)KeQueryPerformanceCounter(NULL).QuadPart;
    r->thread = (ULONG64)(ULONG_PTR)KeGetCurrentThread();
    r->pid = (ULONG)(ULONG_PTR)PsGetCurrentProcessId();
    r->tid = (ULONG)(ULONG_PTR)PsGetCurrentThreadId();
    r->cpu = (USHORT)cpu_index();
    RtlCopyMemory(r + 1, payload, plen);
    stage_put(buf, size);
    s_cap_writing = FALSE;
}

/* Drain committed records of every ring.  keep=FALSE discards them (stopped). */
static void drain(BOOLEAN keep)
{
    ULONG i;
    for (i = 0; i < g.ncpu; i++) {
        NVDDI_RING *r = &g.rings[i];
        LONG64 t = r->tail;
        LONG64 h = InterlockedCompareExchange64(&r->head, 0, 0);
        LONG64 d, db;
        while (t < h) {
            ULONG off = (ULONG)(t & r->mask);
            NDL_REC *rec = (NDL_REC *)(r->buf + off);
            ULONG sz = (ULONG)InterlockedCompareExchange((volatile LONG *)&rec->size, 0, 0);
            if (sz == 0)
                break;                         /* reserved, not committed yet */
            if (sz < 8 || sz > r->size - off) {
                /* cannot happen; resynchronise by dropping the ring content */
                InterlockedExchange(&g.last_error, STATUS_DATA_ERROR);
                RtlZeroMemory(r->buf, r->size);
                t = h;
                break;
            }
            if (keep && rec->type != NDL_T_PAD)
                stage_put(rec, sz);
            RtlZeroMemory(rec, sz);
            t += sz;
        }
        InterlockedExchange64(&r->tail, t);    /* release: space is free again */
        d = r->dropped;
        db = r->dropped_bytes;
        if (keep && (d != r->reported_dropped)) {
            NDL_DROP dr;
            dr.records = (ULONG64)(d - r->reported_dropped);
            dr.bytes = (ULONG64)(db - r->reported_dropped_bytes);
            dr.cpu = i;
            dr.reason = 1;
            put_own(NDL_T_DROP, &dr, sizeof(dr));
        }
        r->reported_dropped = d;
        r->reported_dropped_bytes = db;
    }
}

static void close_file(void)
{
    if (s_file) {
        NDL_END e;
        e.records = s_records;
        e.bytes = (ULONG64)g.file_bytes;
        e.dropped = (ULONG64)g.dropped_file;
        put_own(NDL_T_END, &e, sizeof(e));
        stage_flush();
        {
            IO_STATUS_BLOCK io;
            ZwFlushBuffersFile(s_file, &io);
        }
        ZwClose(s_file);
        s_file = NULL;
    }
}

static NTSTATUS open_file(const WCHAR *dir)
{
    WCHAR path[200];
    UNICODE_STRING u;
    OBJECT_ATTRIBUTES oa;
    IO_STATUS_BLOCK io;
    NTSTATUS st;
    LARGE_INTEGER now, local;
    TIME_FIELDS tf;
    NDL_FILE_HEADER hdr;
    RTL_OSVERSIONINFOW ver;
    char name[96];
    ULONG i, n;

    st = make_dir(dir);
    if (!NT_SUCCESS(st))
        return st;
    KeQuerySystemTime(&now);
    ExSystemTimeToLocalTime(&now, &local);
    RtlTimeToTimeFields(&local, &tf);
    fmt(name, sizeof(name), "\\nvddi-%04u%02u%02u-%02u%02u%02u-b%u-%u.ndl", tf.Year, tf.Month, tf.Day,
        tf.Hour, tf.Minute, tf.Second, g.boot_count, g.file_index);
    for (i = 0; dir[i] && i < 120; i++)
        path[i] = dir[i];
    for (n = 0; name[n] && i < 199; n++, i++)
        path[i] = (WCHAR)name[n];
    path[i] = 0;
    RtlInitUnicodeString(&u, path);
    InitializeObjectAttributes(&oa, &u, OBJ_KERNEL_HANDLE | OBJ_CASE_INSENSITIVE, NULL, NULL);
    st = ZwCreateFile(&s_file, GENERIC_WRITE | SYNCHRONIZE, &oa, &io, NULL, FILE_ATTRIBUTE_NORMAL,
                      FILE_SHARE_READ, FILE_OVERWRITE_IF,
                      FILE_NON_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT | FILE_SEQUENTIAL_ONLY, NULL, 0);
    if (!NT_SUCCESS(st)) {
        s_file = NULL;
        return st;
    }
    RtlCopyMemory(g.file_name, path, (i + 1) * sizeof(WCHAR));
    g.file_index++;
    InterlockedExchange64(&g.file_bytes, 0);
    s_records = 0;
    s_cap_marked = FALSE;

    RtlZeroMemory(&hdr, sizeof(hdr));
    RtlCopyMemory(hdr.magic, NDL_MAGIC, 8);
    hdr.version = NDL_VERSION;
    hdr.header_size = NDL_HEADER_SIZE;
    hdr.qpc_freq = (ULONG64)g.qpc_freq.QuadPart;
    hdr.start_qpc = (ULONG64)KeQueryPerformanceCounter(NULL).QuadPart;
    hdr.start_systime = (ULONG64)now.QuadPart;
    hdr.load_qpc = g.load_qpc;
    hdr.boot_count = g.boot_count;
    hdr.file_index = g.file_index - 1;
    hdr.ncpu = g.ncpu;
    hdr.ring_bytes = g.ring_bytes;
    RtlZeroMemory(&ver, sizeof(ver));
    ver.dwOSVersionInfoSize = sizeof(ver);
    if (NT_SUCCESS(RtlGetVersion(&ver))) {
        hdr.os_major = ver.dwMajorVersion;
        hdr.os_minor = ver.dwMinorVersion;
        hdr.os_build = ver.dwBuildNumber;
    }
    hdr.level = (ULONG)g.level;
    hdr.max_blob = g.max_blob;
    hdr.nvddi_version = NVDDI_VERSION;
    hdr.wdk_ddi_version = DXGKDDI_INTERFACE_VERSION;
    st = write_raw(&hdr, sizeof(hdr));
    if (NT_SUCCESS(st)) {
        /* schema record */
        ULONG size = ALIGN8(sizeof(NDL_REC) + g_nvddi_schema_len + 1);
        NDL_REC *r = (NDL_REC *)ExAllocatePoolWithTag(PagedPool, size, NVDDI_TAG);
        if (r) {
            RtlZeroMemory(r, size);
            r->size = size;
            r->type = NDL_T_SCHEMA;
            r->seq = 0;
            r->qpc = hdr.start_qpc;
            RtlCopyMemory(r + 1, g_nvddi_schema, g_nvddi_schema_len);
            stage_put(r, size);
            ExFreePoolWithTag(r, NVDDI_TAG);
        }
        stage_flush();
    }
    return st;
}

static void health_check(void)
{
    LARGE_INTEGER now;
    ULONG64 secs;
    if (s_healthy_done)
        return;
    now = KeQueryPerformanceCounter(NULL);
    secs = ((ULONG64)now.QuadPart - g.load_qpc) / (ULONG64)g.qpc_freq.QuadPart;
    if (secs >= g.healthy_seconds && !g.start_failed) {
        s_healthy_done = TRUE;
        nvddi_set_fail_count(0);
        log_event(NDL_EV_HEALTHY, 0, secs, 0, "fail counter cleared after %u s", (ULONG)secs);
    }
}

static VOID writer_thread(PVOID ctx)
{
    LARGE_INTEGER timeout, first_try, last_try;
    BOOLEAN fallback = FALSE;
    UNREFERENCED_PARAMETER(ctx);

    timeout.QuadPart = -250000;    /* 25 ms */
    first_try = KeQueryPerformanceCounter(NULL);
    last_try.QuadPart = 0;
    s_last_flush = first_try;
    for (;;) {
        LONG cmd;
        KeWaitForSingleObject(&g.wake, Executive, KernelMode, FALSE, &timeout);
        cmd = InterlockedExchange(&g.cmd, 0);

        if (cmd & WCMD_STOP) {
            drain(s_file != NULL);
            close_file();
            InterlockedExchange(&g.state, ST_STOPPED);
            drain(FALSE);
        }
        if (cmd & WCMD_START) {
            drain(s_file != NULL);
            close_file();
            InterlockedExchange(&g.state, ST_PENDING);
        }
        if (g.state == ST_PENDING && (cmd || (ULONG64)(KeQueryPerformanceCounter(NULL).QuadPart - last_try.QuadPart) >
                                                  (ULONG64)g.qpc_freq.QuadPart / 2)) {
            NTSTATUS st;
            last_try = KeQueryPerformanceCounter(NULL);
            st = open_file(fallback ? L"\\SystemRoot\\nvddi" : g.logdir);
            if (NT_SUCCESS(st)) {
                InterlockedExchange(&g.state, ST_CAPTURE);
                log_event(NDL_EV_FILE, 0, 0, 0, "log file %ls", g.file_name);
                if (g.hooked_drv)
                    log_event(NDL_EV_INFO, 0, (ULONG64)(ULONG_PTR)g.hooked_drv->DriverStart,
                              g.hooked_drv->DriverSize, "hooked %s", g.hooked_name);
            } else {
                LARGE_INTEGER now = KeQueryPerformanceCounter(NULL);
                InterlockedExchange(&g.last_error, st);
                if (!fallback && (ULONG64)(now.QuadPart - first_try.QuadPart) >
                                     30ull * (ULONG64)g.qpc_freq.QuadPart)
                    fallback = TRUE;
            }
        }
        if (g.state == ST_CAPTURE)
            drain(TRUE);
        else if (g.state == ST_STOPPED)
            drain(FALSE);
        stage_flush();
        if (s_file) {
            LARGE_INTEGER now = KeQueryPerformanceCounter(NULL);
            if ((ULONG64)(now.QuadPart - s_last_flush.QuadPart) > 2ull * (ULONG64)g.qpc_freq.QuadPart) {
                IO_STATUS_BLOCK io;
                ZwFlushBuffersFile(s_file, &io);
                s_last_flush = now;
            }
        }
        health_check();
        if (cmd)
            KeSetEvent(&g.cmd_done, IO_NO_INCREMENT, FALSE);
        if (cmd & WCMD_EXIT)
            break;
    }
    close_file();
    PsTerminateSystemThread(STATUS_SUCCESS);
}

NTSTATUS writer_start(void)
{
    HANDLE h;
    NTSTATUS st;
    s_stage = (UCHAR *)ExAllocatePoolWithTag(NonPagedPoolNx, STAGE_BYTES, NVDDI_TAG);
    if (!s_stage)
        return STATUS_INSUFFICIENT_RESOURCES;
    KeInitializeEvent(&g.wake, SynchronizationEvent, FALSE);
    KeInitializeEvent(&g.cmd_done, SynchronizationEvent, FALSE);
    st = PsCreateSystemThread(&h, THREAD_ALL_ACCESS, NULL, NULL, NULL, writer_thread, NULL);
    if (!NT_SUCCESS(st))
        return st;
    ObReferenceObjectByHandle(h, THREAD_ALL_ACCESS, *PsThreadType, KernelMode, (PVOID *)&g.writer, NULL);
    ZwClose(h);
    return STATUS_SUCCESS;
}

/* PASSIVE_LEVEL; waits for the writer to process the command */
NTSTATUS writer_command(LONG cmd, ULONG timeout_ms)
{
    LARGE_INTEGER to;
    KeClearEvent(&g.cmd_done);
    InterlockedOr(&g.cmd, cmd);
    KeSetEvent(&g.wake, IO_NO_INCREMENT, FALSE);
    to.QuadPart = -(LONG64)timeout_ms * 10000;
    return KeWaitForSingleObject(&g.cmd_done, Executive, KernelMode, FALSE, &to);
}
