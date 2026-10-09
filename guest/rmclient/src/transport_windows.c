/* SPDX-License-Identifier: MIT */
/*
 * librmclient Windows transport: RM escapes through Conduit's display miniport
 * (the Helios KMD), as HELIOS_ESCAPE_NVRM calls on D3DKMTEscape.
 *
 * What the KMD does and does not do (guest/windows/protocol/src/nvrm.rs is the
 * contract): it forwards a request VERBATIM to the host's RM backend and returns
 * the reply, records which backend handles this process opened, and refuses what
 * a process must not be able to say (another process's handle, a page-run table).
 * It does not interpret RM. So the part the Linux guest module does inside its
 * kernel — turning an NVIDIA escape and the blocks its pointers address into the
 * wire message — is done HERE, in user mode, by the code below. The wire format
 * is in win_wire.h and is unit-tested on any host.
 *
 * A "transport fd" is the backend handle the host returned from Open, so the fd
 * values librmclient writes into payloads (register_fd.ctl_fd, event fds, the
 * export fd) are already what the host understands and need no rewriting.
 *
 * Implemented: open, close and ioctl, including the nested parameter block of
 * NV_ESC_RM_CONTROL and NV_ESC_RM_ALLOC; CPU mapping (map_memory/unmap_memory,
 * over HELIOS_NVRM_OP_MMAP/MUNMAP); alloc_pages (VirtualAlloc).
 * Not yet, each answering -ENOSYS until the matching KMD verb exists (the ABI
 * reserves them): OS events (event_wait -> EVENT_REGISTER) and registering user
 * memory as an OS descriptor (PIN). Controls whose parameters hold a pointer of
 * their own (a "deep" block) are sent without it; RM then answers its own error
 * for them.
 *
 * Beyond the transport vtable, crm_win_open_device / crm_win_ioctl /
 * crm_win_scanout_flip (rmclient_transport.h) send what presenting RM memory
 * takes: an Open of a DRM render node, a DRM ioctl with its NVKMS block as the
 * nested block, and a ScanoutFlip. crm_win_escape_raw sends any other Helios
 * escape verb as the caller built it (tests: FOREIGN_RESOURCE refusals).
 *
 * CPU mapping follows Linux's protocol, which the host's backend ties to a
 * channel: open a fresh channel, NV_ESC_RM_MAP_MEMORY on the control channel
 * naming it, then map that channel at offset 0 for the page-rounded length. The
 * KMD does the last step (MMAP), and the channel is kept until unmap, because RM
 * and the backend refuse a second mapping on it.
 */
#include "rmclient_transport.h"

#if defined(_WIN32)

#include <stdarg.h>
#include <stddef.h>
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <wchar.h>

#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#include <windows.h>
/* d3dkmthk.h's prototypes return NTSTATUS but the header pulls no header that
 * defines it, and mingw's <windows.h> does not either. Provide it, guarded by the
 * SDK's _NTDEF_ so it is never defined twice. The build adds the vendored WDK
 * headers (guest/windows/icd/win-build/wdk-include) to the include path. */
#ifndef _NTDEF_
typedef LONG NTSTATUS, *PNTSTATUS;
#endif
#include <d3dkmthk.h>

#include "helios_nvrm_escape.h"
#include "nv_ioctl_defs.h"
#include "win_gen.h"
#include "win_wire.h"

/* ---- device loss: KMD views that vanish under a live process -------------
 *
 * The CPU mappings below are views the KMD maps into this process itself
 * (HELIOS_NVRM_OP_MMAP: USERD, GPFIFO rings, semaphores, BAR memory). When
 * the KMD stops under live processes (a live driver update, a device
 * restart), dxgkrnl destroys the devices and the KMD unmaps those views; the
 * next plain load or store from NVK is an access violation (an NVK process
 * died writing GP_PUT into USERD at a device restart). helios_kmdmap.h is
 * the one process-wide table and exception-handler protocol the Venus ICD
 * and the Helios UMD use for the same problem (see there): every view is
 * registered, a vanished one is backed with zero pages, and the process's
 * loss epoch moves, which NVK reads through crm_win_loss_epoch(). After a
 * loss this library sends no escape with the dead generation's handles; the
 * next open starts a new generation on the KMD that came back
 * (generation_restart), so a long-lived process gets the GPU back through RM
 * without restarting. */
static void crm_kmdmap_log(const char *fmt, ...)
{
    FILE *f = fopen("C:\\ProgramData\\Helios\\helios_icd_diag.log", "a");
    if (!f)
        return;
    fprintf(f, "%lu pid=%lu rmclient ", (unsigned long)GetTickCount(),
            (unsigned long)GetCurrentProcessId());
    va_list ap;
    va_start(ap, fmt);
    vfprintf(f, fmt, ap);
    va_end(ap);
    fputc('\n', f);
    fclose(f);
}
#define HELIOS_KMDMAP_LOG(...) crm_kmdmap_log(__VA_ARGS__)
#include "helios_kmdmap.h"

/* ---- D3DKMT, resolved at run time so the build needs no gdi32 import ------ */

typedef NTSTATUS(APIENTRY *pfn_enum_adapters2)(D3DKMT_ENUMADAPTERS2 *);
typedef NTSTATUS(APIENTRY *pfn_create_device)(D3DKMT_CREATEDEVICE *);
typedef NTSTATUS(APIENTRY *pfn_destroy_device)(const D3DKMT_DESTROYDEVICE *);
typedef NTSTATUS(APIENTRY *pfn_create_context)(D3DKMT_CREATECONTEXT *);
typedef NTSTATUS(APIENTRY *pfn_destroy_context)(const D3DKMT_DESTROYCONTEXT *);
typedef NTSTATUS(APIENTRY *pfn_close_adapter)(const D3DKMT_CLOSEADAPTER *);
typedef NTSTATUS(APIENTRY *pfn_escape)(const D3DKMT_ESCAPE *);
typedef NTSTATUS(APIENTRY *pfn_query_adapter_info)(const D3DKMT_QUERYADAPTERINFO *);

struct win_ctx {
    SRWLOCK lock;
    int ready;

    /* Device loss (helios_kmdmap.h): attached to the shared table once, at
     * the first successful init. `gen` is this generation's record (win_gen.h):
     * the table epoch it attached at, which is the one loss latch, and the
     * transport epoch QUERY_CAPS gave. */
    int kmdmap_attached;
    struct crm_win_gen gen;

    /* Wakes every thread blocked in event_wait when the transport is lost:
     * set by win_mark_lost, and by the KMD through the TRANSPORT_LOST
     * registration (handle 0) when it resets the device. One manual-reset event
     * for the life of the process (closing it under a waiter would be a race);
     * generation_restart resets it and the next init registers it again with
     * the KMD that came back. */
    HANDLE lost_ev;
    int events_ok;       /* QUERY_CAPS: EVENT_REGISTER and the TRANSPORT_LOST kind usable */
    int lost_registered; /* this generation's KMD holds our TRANSPORT_LOST registration */
    volatile LONG lost_logged; /* the loss of this generation was logged */

    pfn_enum_adapters2 enum_adapters2;
    pfn_create_device create_device;
    pfn_destroy_device destroy_device;
    pfn_create_context create_context;
    pfn_destroy_context destroy_context;
    pfn_close_adapter close_adapter;
    pfn_escape escape;
    pfn_query_adapter_info query_adapter_info;

    D3DKMT_HANDLE adapter;
    D3DKMT_HANDLE device;
    D3DKMT_HANDLE context;

    uint32_t max_buffer; /* QUERY_CAPS.max_buffer_bytes */
    uint64_t supported_ops; /* QUERY_CAPS.supported_ops: bit n = HELIOS_NVRM_OP n;
                               bits 32..63 HELIOS_NVRM_CAP_* */
    uint32_t device_features; /* QUERY_CAPS.device_features (NVGPU_CFG_*) */
    uint64_t foreign_ops;   /* FOREIGN_RESOURCE QUERY_CAPS.supported_ops, 0 until asked */
    uint64_t caps_epoch; /* the transport epoch of the last accepted QUERY_CAPS */
    LUID luid;           /* the chosen adapter's LUID (D3DKMTEnumAdapters2) */

    /* Host's per-class allocation parameter sizes (GetSysFiles section 3). */
    uint32_t *alloc_pairs; /* {class, size} * n_alloc */
    uint32_t n_alloc;

    /* Live CPU mappings made by map_memory, found again by pointer at unmap. */
    struct win_map {
        void *ptr;        /* the address handed to the caller */
        void *base;       /* the start of the KMD's view (registered for loss) */
        int fd;           /* the channel kept for this mapping */
        uint32_t id;      /* the KMD/host mapping id */
        uint32_t gen;     /* the generation that made it */
    } *maps;
    uint32_t n_maps, cap_maps;

    /* Channels of CPU mappings whose view and host mapping are gone but whose
     * NV_ESC_RM_UNMAP_MEMORY has not been issued yet: RM ties the mapping to the
     * channel, so the channel must outlive that call (Linux closes it after). Closed
     * when the matching RM_UNMAP_MEMORY returns, or with the control channel. */
    struct win_pend {
        uint32_t memory;
        uint64_t cookie;
        int fd;
    } *pend;
    uint32_t n_pend, cap_pend;
    int ctl_handle; /* the control channel, -1 until opened */

    /* One manual-reset event per channel that has been waited on: the KMD signals
     * it when the host reports the channel readable (HELIOS_NVRM_OP_EVENT_REGISTER). */
    struct win_ev {
        int fd;
        HANDLE ev;
    } *evs;
    uint32_t n_evs, cap_evs;

    /* The SCANOUT_RELEASED registration (kind 3, handle 0), made on first use
     * by crm_win_scanout_wait_released: an auto-reset event, under `lock`. */
    HANDLE release_ev;

    /* Generations (see generation_restart and win_gen.h): gen.generation is 1
     * from the first init, +1 each time the KMD came back after a loss and the
     * library reopened it. */
    /* Channels opened in this generation (win_open, crm_win_open_device) and
     * channels of earlier generations still open in some caller: the latter
     * are refused without an escape and only forgotten at their close. */
    int *live_fds;
    uint32_t n_live, cap_live;
    int *stale_fds;
    uint32_t n_stale, cap_stale;
};

static struct win_ctx g_ctx = { .lock = SRWLOCK_INIT, .ctl_handle = -1 };

#define STATUS_NOT_IMPLEMENTED_NT ((NTSTATUS)0xC0000002L)

static int nt_to_errno(NTSTATUS st)
{
    if (st == STATUS_NOT_IMPLEMENTED_NT)
        return -ENOSYS;
    if (st == (NTSTATUS)0xC000000DL) /* STATUS_INVALID_PARAMETER */
        return -EINVAL;
    if (st == (NTSTATUS)0xC00000A3L || st == (NTSTATUS)0xC00000BBL)
        return -ENODEV; /* STATUS_DEVICE_NOT_READY / NOT_SUPPORTED */
    return -EIO;
}

static int kmd_status_to_errno(int32_t status)
{
    switch (status) {
    case HELIOS_NVRM_ST_OK:
        return 0;
    case HELIOS_NVRM_ST_NOT_OWNED:
        return -EBADF;
    case HELIOS_NVRM_ST_MSG_TYPE_REFUSED:
    case HELIOS_NVRM_ST_FORBIDDEN:
        return -EPERM;
    case HELIOS_NVRM_ST_TRANSPORT_RESET:
        return -ENODEV;
    case HELIOS_NVRM_ST_NO_RESOURCES:
        return -EMFILE;
    case HELIOS_NVRM_ST_UNSUPPORTED:
        return -ENOSYS;
    case HELIOS_NVRM_ST_TIMEOUT:
        return -ETIMEDOUT;
    case HELIOS_NVRM_ST_BAD_RANGE:
        return -EINVAL;
    case HELIOS_NVRM_ST_SCANOUT_BUSY:
        return -EBUSY;
    case HELIOS_NVRM_ST_NO_SOURCE:
        return -ENOENT;
    default:
        return -EIO;
    }
}

/* One HELIOS_ESCAPE_NVRM call on an already-built buffer. Returns 0 and leaves
 * the KMD's verdict in head->status, or a negative errno for a transport
 * failure (an older KMD without the verb answers STATUS_NOT_IMPLEMENTED). */
/*
 * CRM_WIN_PROF_FILE=path ("%p" in it becomes the process id): count and time every escape by kind, from any
 * thread, and rewrite `path` with the cumulative table at most once a second
 * (so a killed process still leaves one; diff two snapshots for a rate).
 * Kinds: op (QUERY_CAPS/MMAP, MUNMAP, EVENT_REGISTER, PIN, ...), FORWARD by message type,
 * Ioctl by NVIDIA escape number, and NV_ESC_RM_CONTROL (0x2a) by control cmd.
 * Off (one predictable branch per call) unless the variable is set.
 */
#define PROF_SLOTS 1024
struct prof_ent {
    uint64_t key; /* kind << 32 | sub; 0 = free */
    uint64_t n;
    double sum_us, max_us;
};
static struct prof_ent g_prof[PROF_SLOTS];
static SRWLOCK g_prof_lock = SRWLOCK_INIT;
static int g_prof_on = -1;
static char g_prof_path[MAX_PATH];
static LARGE_INTEGER g_prof_freq, g_prof_t0, g_prof_last;

enum { PROF_OP = 1, PROF_MSG = 2, PROF_IOCTL = 3, PROF_CTRL = 4, PROF_WAIT = 5 };

static uint64_t prof_key(const void *buf)
{
    const HeliosNvrmHeader *h = buf;
    if (h->op != HELIOS_NVRM_OP_FORWARD)
        return ((uint64_t)PROF_OP << 32) | h->op;
    const uint8_t *req = (const uint8_t *)buf + HELIOS_NVRM_FORWARD_REQ_OFFSET;
    const uint32_t type = crm_get32(req);
    if (type != CRM_WIRE_MSG_IOCTL)
        return ((uint64_t)PROF_MSG << 32) | type;
    const uint32_t nr = crm_get32(req + CRM_WIRE_HDR) & 0xffu;
    const uint32_t data_len = crm_get32(req + CRM_WIRE_HDR + 4);
    if (nr == 0x2a && data_len >= 12) /* NVOS54_PARAMETERS.cmd at offset 8 */
        return ((uint64_t)PROF_CTRL << 32) |
               crm_get32(req + CRM_WIRE_HDR + CRM_WIRE_IOCTL_REQ + 8);
    return ((uint64_t)PROF_IOCTL << 32) | nr;
}

static void prof_write(double now_ms)
{
    FILE *f = fopen(g_prof_path, "w");
    if (!f)
        return;
    fprintf(f, "# t_ms %.1f pid %lu\nkind,sub,calls,total_us,max_us\n", now_ms,
            (unsigned long)GetCurrentProcessId());
    static const char *kinds[] = { "?", "op", "msg", "ioctl", "control", "evwait", "?", "?" };
    for (int i = 0; i < PROF_SLOTS; i++) {
        const struct prof_ent *e = &g_prof[i];
        if (!e->key)
            continue;
        fprintf(f, "%s,0x%x,%llu,%.1f,%.1f\n", kinds[(e->key >> 32) & 7],
                (unsigned)(e->key & 0xffffffffu), (unsigned long long)e->n, e->sum_us,
                e->max_us);
    }
    fclose(f);
}

static void prof_record(uint64_t key, LARGE_INTEGER t0, LARGE_INTEGER t1)
{
    const double us = (double)(t1.QuadPart - t0.QuadPart) * 1e6 / (double)g_prof_freq.QuadPart;
    AcquireSRWLockExclusive(&g_prof_lock);
    uint32_t h = (uint32_t)(key ^ (key >> 29)) * 2654435761u;
    for (int probe = 0; probe < PROF_SLOTS; probe++) {
        struct prof_ent *e = &g_prof[(h + probe) % PROF_SLOTS];
        if (e->key != key && e->key)
            continue;
        e->key = key;
        e->n++;
        e->sum_us += us;
        if (us > e->max_us)
            e->max_us = us;
        break;
    }
    if (t1.QuadPart - g_prof_last.QuadPart > g_prof_freq.QuadPart) {
        g_prof_last = t1;
        prof_write((double)(t1.QuadPart - g_prof_t0.QuadPart) * 1e3 / (double)g_prof_freq.QuadPart);
    }
    ReleaseSRWLockExclusive(&g_prof_lock);
}

static int nvrm_escape_raw(struct win_ctx *c, void *buf, uint32_t size);

static int nvrm_escape(struct win_ctx *c, void *buf, uint32_t size)
{
    if (g_prof_on < 0) {
        char raw[MAX_PATH];
        const DWORD n = GetEnvironmentVariableA("CRM_WIN_PROF_FILE", raw, sizeof(raw));
        QueryPerformanceFrequency(&g_prof_freq);
        QueryPerformanceCounter(&g_prof_t0);
        g_prof_last = g_prof_t0;
        g_prof_on = n > 0 && n < sizeof(raw);
        if (g_prof_on) {
            /* "%p" becomes the process id, so one machine-wide setting gives
             * every process its own table (Steam, the game, DWM). */
            size_t o = 0;
            for (DWORD i = 0; i < n && o + 12 < sizeof(g_prof_path); i++) {
                if (raw[i] == '%' && i + 1 < n && raw[i + 1] == 'p') {
                    o += (size_t)snprintf(g_prof_path + o, sizeof(g_prof_path) - o, "%lu",
                                          (unsigned long)GetCurrentProcessId());
                    i++;
                } else {
                    g_prof_path[o++] = raw[i];
                }
            }
            g_prof_path[o] = 0;
        }
    }
    if (!g_prof_on)
        return nvrm_escape_raw(c, buf, size);
    const uint64_t key = prof_key(buf);
    LARGE_INTEGER t0, t1;
    QueryPerformanceCounter(&t0);
    const int r = nvrm_escape_raw(c, buf, size);
    QueryPerformanceCounter(&t1);
    prof_record(key, t0, t1);
    return r;
}

/* Has the KMD gone away under this process since this generation began? */
static int win_lost(struct win_ctx *c)
{
    return c->kmdmap_attached && helios_kmdmap_lost(c->gen.loss_epoch0);
}

/* This generation's transport is gone: move the loss table (once per
 * generation, helios_kmdmap_mark_lost) and wake every blocked event_wait. The
 * next open starts a new generation (generation_restart). */
static void win_mark_lost(struct win_ctx *c, const char *why, unsigned long detail)
{
    if (!c->kmdmap_attached)
        return;
    if (InterlockedExchange(&c->lost_logged, 1) == 0)
        crm_kmdmap_log("device-lost: %s 0x%08lx in generation %u (transport epoch 0x%llx); "
                       "no more RM escapes until the next open starts a new generation",
                       why, detail, c->gen.generation, (unsigned long long)c->gen.init_epoch);
    helios_kmdmap_mark_lost(c->gen.loss_epoch0);
    if (c->lost_ev)
        SetEvent(c->lost_ev);
}

/* One D3DKMTEscape of `buf` on this generation's device, with the loss rules
 * below. *nt (when not NULL) gets the D3DKMTEscape NTSTATUS. */
static int escape_judged(struct win_ctx *c, void *buf, uint32_t size, int32_t *nt)
{
    /* Nothing of a lost generation reaches the KMD, not even a release: ids
     * restart per transport, so an old MUNMAP / UNPIN / EVENT_UNREGISTER could
     * tear down a new object with the same id. */
    if (win_lost(c))
        return -ENODEV;

    D3DKMT_ESCAPE esc;
    memset(&esc, 0, sizeof(esc));
    esc.hAdapter = c->adapter;
    esc.hDevice = c->device;
    esc.hContext = c->context;
    esc.Type = D3DKMT_ESCAPE_DRIVERPRIVATE;
    esc.Flags.HardwareAccess = 0; /* none of this touches hardware: no exclusive adapter lock */
    esc.pPrivateDriverData = buf;
    esc.PrivateDriverDataSize = size;
    const NTSTATUS st = c->escape(&esc);
    if (nt)
        *nt = (int32_t)st;
    if (st != 0) {
        if (c->ready && (helios_kmdmap_status_is_device_gone(st) ||
                         helios_nvrm_ntstatus_is_lost((int32_t)st))) {
            win_mark_lost(c, "D3DKMTEscape status", (unsigned long)st);
            return -ENODEV;
        }
        return nt_to_errno(st);
    }
    /* A reply from another transport (a changed epoch: another StartDevice or,
     * with the KMD's per-image salt, a reloaded driver image) or a reset verdict
     * is not an answer to what was asked: the handles it named are gone. `ready`
     * is set only after init accepted the epoch, so init's own QUERY_CAPS is not
     * judged. */
    const HeliosNvrmHeader *h = (const HeliosNvrmHeader *)buf;
    if (c->ready && crm_win_gen_reply_lost(&c->gen, buf, size)) {
        win_mark_lost(c, h->status == HELIOS_NVRM_ST_TRANSPORT_RESET ? "TRANSPORT_RESET, epoch"
                                                                      : "reply from another transport, epoch",
                      (unsigned long)h->epoch);
        return -ENODEV;
    }
    return 0;
}

static int nvrm_escape_raw(struct win_ctx *c, void *buf, uint32_t size)
{
    return escape_judged(c, buf, size, NULL);
}

/*
 * FORWARD: send `req` (MsgHeader | payload) and copy the reply into `resp`.
 * Returns 0 with *resp_len set, or a negative errno. The host's own verdict is
 * the reply's MsgHeader.status, which the callers read.
 */
static int win_forward(struct win_ctx *c, const void *req, uint32_t req_len, void *resp,
                       uint32_t resp_cap, uint32_t *resp_len, uint32_t pin_id,
                       uint32_t rm_status_off)
{
    const size_t resp_off = helios_nvrm_forward_resp_offset(req_len);
    const size_t total = resp_off + resp_cap;
    if (total > c->max_buffer)
        return -EFBIG;
    uint8_t *buf = calloc(1, total);
    if (!buf)
        return -ENOMEM;
    HeliosNvrmForward *f = (HeliosNvrmForward *)buf;
    helios_nvrm_init(&f->head, HELIOS_NVRM_OP_FORWARD, (uint32_t)total);
    f->req_len = req_len;
    f->resp_cap = resp_cap;
    f->pin_id = pin_id;
    f->rm_status_off = rm_status_off;
    memcpy(buf + HELIOS_NVRM_FORWARD_REQ_OFFSET, req, req_len);

    int r = nvrm_escape(c, buf, (uint32_t)total);
    if (r == 0)
        r = kmd_status_to_errno(f->head.status);
    if (r == 0) {
        uint32_t n = f->resp_len;
        if (n > resp_cap)
            n = resp_cap;
        memcpy(resp, buf + resp_off, n);
        *resp_len = n;
    }
    free(buf);
    return r;
}

/* ---- initialisation: find the adapter, probe the verb, read the tables ---- */

static int resolve_api(struct win_ctx *c)
{
    HMODULE gdi = LoadLibraryA("gdi32.dll");
    if (!gdi)
        return -ENOSYS;
    c->enum_adapters2 = (pfn_enum_adapters2)(void *)GetProcAddress(gdi, "D3DKMTEnumAdapters2");
    c->create_device = (pfn_create_device)(void *)GetProcAddress(gdi, "D3DKMTCreateDevice");
    c->destroy_device = (pfn_destroy_device)(void *)GetProcAddress(gdi, "D3DKMTDestroyDevice");
    c->create_context = (pfn_create_context)(void *)GetProcAddress(gdi, "D3DKMTCreateContext");
    c->destroy_context = (pfn_destroy_context)(void *)GetProcAddress(gdi, "D3DKMTDestroyContext");
    c->close_adapter = (pfn_close_adapter)(void *)GetProcAddress(gdi, "D3DKMTCloseAdapter");
    c->escape = (pfn_escape)(void *)GetProcAddress(gdi, "D3DKMTEscape");
    c->query_adapter_info =
        (pfn_query_adapter_info)(void *)GetProcAddress(gdi, "D3DKMTQueryAdapterInfo");
    if (!c->enum_adapters2 || !c->create_device || !c->close_adapter || !c->escape)
        return -ENOSYS;
    return 0;
}

static void close_handles(struct win_ctx *c, D3DKMT_HANDLE adapter, D3DKMT_HANDLE device,
                          D3DKMT_HANDLE context)
{
    if (context && c->destroy_context) {
        D3DKMT_DESTROYCONTEXT dc;
        memset(&dc, 0, sizeof(dc));
        dc.hContext = context;
        (void)c->destroy_context(&dc);
    }
    if (device && c->destroy_device) {
        D3DKMT_DESTROYDEVICE dd;
        memset(&dd, 0, sizeof(dd));
        dd.hDevice = device;
        (void)c->destroy_device(&dd);
    }
    if (adapter) {
        D3DKMT_CLOSEADAPTER ca;
        memset(&ca, 0, sizeof(ca));
        ca.hAdapter = adapter;
        (void)c->close_adapter(&ca);
    }
}

/* Open a device on `adapter` and ask the KMD for QUERY_CAPS. Only the Helios
 * KMD answers it (foreign adapters reject the private escape), so success
 * identifies the adapter — the registry description is only a hint. */
static int probe_adapter(struct win_ctx *c, D3DKMT_HANDLE adapter, D3DKMT_HANDLE *out_device,
                         D3DKMT_HANDLE *out_context)
{
    D3DKMT_CREATEDEVICE cd;
    memset(&cd, 0, sizeof(cd));
    cd.hAdapter = adapter;
    if (c->create_device(&cd) != 0)
        return -ENODEV;
    D3DKMT_HANDLE context = 0;
    if (c->create_context) {
        D3DKMT_CREATECONTEXT cc;
        memset(&cc, 0, sizeof(cc));
        cc.hDevice = cd.hDevice;
        if (c->create_context(&cc) == 0)
            context = cc.hContext;
    }

    c->adapter = adapter;
    c->device = cd.hDevice;
    c->context = context;

    HeliosNvrmQueryCaps caps;
    memset(&caps, 0, sizeof(caps));
    helios_nvrm_init(&caps.head, HELIOS_NVRM_OP_QUERY_CAPS, sizeof(caps));
    int r = nvrm_escape(c, &caps, sizeof(caps));
    if (r == 0 && caps.head.status == HELIOS_NVRM_ST_OK &&
        (caps.supported_ops & (1ull << HELIOS_NVRM_OP_FORWARD))) {
        /* Every call needs room for the FORWARD struct, a request and a reply. */
        if (caps.max_buffer_bytes < 4096)
            goto reject;
        /* Epoch 0 is the KMD saying there is no transport (a live one is never 0):
         * nothing could be forwarded, and there would be no generation to watch.
         * Not latched: the next open tries again. */
        if (caps.head.epoch == 0) {
            r = -ENODEV;
            goto reject;
        }
        c->max_buffer = caps.max_buffer_bytes;
        c->supported_ops = caps.supported_ops;
        c->device_features = caps.device_features;
        c->caps_epoch = caps.head.epoch;
        c->events_ok = (caps.supported_ops & (1ull << HELIOS_NVRM_OP_EVENT_REGISTER)) != 0 &&
                       (caps.supported_event_kinds & (1u << HELIOS_NVRM_EVENT_TRANSPORT_LOST)) != 0;
        *out_device = cd.hDevice;
        *out_context = context;
        return 0;
    }
reject:
    c->adapter = c->device = c->context = 0;
    if (context && c->destroy_context) {
        D3DKMT_DESTROYCONTEXT dc;
        memset(&dc, 0, sizeof(dc));
        dc.hContext = context;
        (void)c->destroy_context(&dc);
    }
    if (c->destroy_device) {
        D3DKMT_DESTROYDEVICE dd;
        memset(&dd, 0, sizeof(dd));
        dd.hDevice = cd.hDevice;
        (void)c->destroy_device(&dd);
    }
    return r ? r : -ENODEV;
}

static int find_adapter(struct win_ctx *c)
{
    D3DKMT_ENUMADAPTERS2 ea;
    memset(&ea, 0, sizeof(ea));
    if (c->enum_adapters2(&ea) != 0 || ea.NumAdapters == 0)
        return -ENODEV;
    ea.pAdapters = calloc(ea.NumAdapters, sizeof(D3DKMT_ADAPTERINFO));
    if (!ea.pAdapters)
        return -ENOMEM;
    if (c->enum_adapters2(&ea) != 0) {
        free(ea.pAdapters);
        return -ENODEV;
    }

    int found = -ENODEV;
    D3DKMT_HANDLE chosen = 0;
    for (UINT i = 0; i < ea.NumAdapters; i++) {
        const D3DKMT_HANDLE h = ea.pAdapters[i].hAdapter;
        D3DKMT_HANDLE device = 0, context = 0;

        /* Do not send a private escape to a vendor's driver if the adapter can be
         * told apart by name first: probe the Helios / virtio-gpu ones, and every
         * adapter only if the name could not be read (that query is unreliable on
         * some boots). The same rule as the Mesa ICD's adapter discovery. */
        int query_ok = 0, name_match = 0;
        if (c->query_adapter_info) {
            D3DKMT_ADAPTERREGISTRYINFO reg;
            memset(&reg, 0, sizeof(reg));
            D3DKMT_QUERYADAPTERINFO qai;
            memset(&qai, 0, sizeof(qai));
            qai.hAdapter = h;
            qai.Type = KMTQAITYPE_ADAPTERREGISTRYINFO;
            qai.pPrivateDriverData = &reg;
            qai.PrivateDriverDataSize = sizeof(reg);
            query_ok = c->query_adapter_info(&qai) == 0;
            if (query_ok)
                name_match = wcsstr(reg.AdapterString, L"Helios") != NULL ||
                             wcsstr(reg.AdapterString, L"VIRTIO GPU") != NULL;
        }
        const int try_candidate = chosen == 0 && (name_match || !query_ok);
        if (try_candidate && probe_adapter(c, h, &device, &context) == 0) {
            chosen = h;
            c->luid = ea.pAdapters[i].AdapterLuid;
            found = 0;
            continue;
        }
        /* EnumAdapters2 opened every adapter; release the ones we don't keep. */
        close_handles(c, h, 0, 0);
    }
    free(ea.pAdapters);
    return found;
}

/* The tables the host publishes with GetSysFiles. Only the allocation sizes are
 * used here; a missing or malformed stream just leaves the table empty. */
static void read_host_tables(struct win_ctx *c)
{
    const uint32_t cap = 128u * 1024u;
    uint8_t req[CRM_WIRE_HDR];
    uint8_t *resp = malloc(cap);
    if (!resp)
        return;
    const size_t req_len = crm_wire_get_sys_files(req);
    uint32_t n = 0;
    if (win_forward(c, req, (uint32_t)req_len, resp, cap, &n, 0, 0) == 0 && n > 0) {
        /* The reply is the bare stream: GetSysFiles (like GetProcFiles) is the one
         * answer with no MsgHeader in front (host messages.rs, FileEntry). */
        uint32_t pairs[2 * 256];
        const uint32_t found = crm_wire_parse_alloc_sizes(resp, n, pairs, 256);
        if (found) {
            c->alloc_pairs = malloc((size_t)found * 2 * sizeof(uint32_t));
            if (c->alloc_pairs) {
                memcpy(c->alloc_pairs, pairs, (size_t)found * 2 * sizeof(uint32_t));
                c->n_alloc = found;
            }
        }
    }
    free(resp);
}

/* ---- generations: the KMD came back after a loss ------------------------ */

static int fd_list_add(int **list, uint32_t *n, uint32_t *cap, int fd)
{
    if (*n == *cap) {
        const uint32_t ncap = *cap ? *cap * 2 : 16;
        int *nl = realloc(*list, (size_t)ncap * sizeof(int));
        if (!nl)
            return -ENOMEM;
        *list = nl;
        *cap = ncap;
    }
    (*list)[(*n)++] = fd;
    return 0;
}

static int fd_list_remove(int *list, uint32_t *n, int fd)
{
    for (uint32_t i = 0; i < *n; i++) {
        if (list[i] == fd) {
            list[i] = list[--*n];
            return 1;
        }
    }
    return 0;
}

static void fd_track(struct win_ctx *c, int fd)
{
    AcquireSRWLockExclusive(&c->lock);
    (void)fd_list_add(&c->live_fds, &c->n_live, &c->cap_live, fd);
    ReleaseSRWLockExclusive(&c->lock);
}

/* Is `fd` a channel of an earlier generation? Those are dead on the host
 * (the device reset closed every file) and must not reach the new KMD. */
static int fd_stale(struct win_ctx *c, int fd)
{
    if (c->n_stale == 0) /* the common case, no lock */
        return 0;
    int r = 0;
    AcquireSRWLockShared(&c->lock);
    for (uint32_t i = 0; i < c->n_stale; i++) {
        if (c->stale_fds[i] == fd) {
            r = 1;
            break;
        }
    }
    ReleaseSRWLockShared(&c->lock);
    return r;
}

/*
 * The KMD went away under this process (a live driver update, a device
 * restart) and the library was lost (helios_kmdmap.h). A process used to stay
 * lost for good, so a long-lived process (explorer, the shell hosts, DWM)
 * fell back to Venus at its next device creation and stayed there. Now the
 * next open after a loss starts a new generation: the old generation's KMD
 * objects are released, its channels become stale (refused without an escape,
 * forgotten at their close; the host closed them at its reset, which freed
 * every RM object made on them, and the host never reuses a handle number
 * across a reset), its CPU views stay in the loss table (zero pages for
 * whoever still holds a pointer) and are only dropped from it at their unmap,
 * and the adapter is opened again. NVK devices created before the loss stay
 * lost (their loss epoch moved); devices created afterwards use the new
 * generation. Called with c->lock held exclusively and c->ready set.
 */
static void generation_restart(struct win_ctx *c)
{
    const uint32_t old_gen = c->gen.generation;
    const int32_t epoch =
        helios_kmdmap_t ? (int32_t)InterlockedCompareExchange(&helios_kmdmap_t->epoch, 0, 0) : 0;

    /* Channel events: the KMD that held the registrations is gone. */
    for (uint32_t i = 0; i < c->n_evs; i++)
        CloseHandle(c->evs[i].ev);
    c->n_evs = 0;
    if (c->release_ev) {
        CloseHandle(c->release_ev);
        c->release_ev = NULL;
    }
    /* Channels parked for their RM_UNMAP_MEMORY and every live channel: stale. */
    for (uint32_t i = 0; i < c->n_pend; i++)
        (void)fd_list_remove(c->live_fds, &c->n_live, c->pend[i].fd);
    c->n_pend = 0;
    const uint32_t moved = c->n_live;
    for (uint32_t i = 0; i < c->n_live; i++)
        (void)fd_list_add(&c->stale_fds, &c->n_stale, &c->cap_stale, c->live_fds[i]);
    c->n_live = 0;
    c->ctl_handle = -1;
    /* The dead device's D3DKMT objects (dxgkrnl keeps their user handles valid
     * until they are closed). */
    close_handles(c, c->adapter, c->device, c->context);
    c->adapter = c->device = c->context = 0;
    free(c->alloc_pairs);
    c->alloc_pairs = NULL;
    c->n_alloc = 0;
    c->foreign_ops = 0;
    /* The old generation's CPU views are dead: the library never reads, writes
     * or unmaps them again (the KMD that made them is gone; a MUNMAP would reach
     * the new one). They are forgotten here; whatever is at their addresses (the
     * loss table's zero backing for a pointer someone still holds) stays until
     * the process exits, and a later unmap of one is a no-op. */
    const uint32_t dropped = c->n_maps;
    c->n_maps = 0;
    /* The loss event: left SET, so anyone still blocked on the old generation
     * wakes (a set-then-reset pulse can be missed by a waiter in an APC). The
     * first wait of the new generation sees it set, asks the transport, finds
     * it alive and resets it (win_event_wait); win_init registers it again with
     * the KMD that came back. */
    if (c->lost_ev)
        SetEvent(c->lost_ev);
    c->lost_registered = 0;
    c->events_ok = 0;
    c->ready = 0;
    crm_win_gen_restart(&c->gen, epoch);
    InterlockedExchange(&c->lost_logged, 0);
    crm_kmdmap_log("generation %u -> %u: the KMD came back after a loss (loss epoch %d); "
                   "%u channel(s) of generation %u are stale, %u CPU view(s) dropped",
                   old_gen, c->gen.generation, (int)epoch, moved, old_gen, dropped);
}

static int nvrm_event_call_kind(struct win_ctx *c, uint32_t op, uint32_t fd, uint32_t kind,
                                HANDLE ev);

/* Register the process's loss event as this generation's TRANSPORT_LOST event
 * (handle 0: the KMD keys it by process). The KMD then signals it when it
 * resets the device, which wakes every blocked event_wait. Without it (no event
 * queue in this KMD) the loss is still seen through every reply's epoch, and
 * win_mark_lost still wakes the waiters when any thread notices. Not fatal.
 * Needs `ready`; called with c->lock held. */
static void register_lost_event(struct win_ctx *c)
{
    if (!c->events_ok || !c->lost_ev || c->lost_registered)
        return;
    if (nvrm_event_call_kind(c, HELIOS_NVRM_OP_EVENT_REGISTER, 0, HELIOS_NVRM_EVENT_TRANSPORT_LOST,
                             c->lost_ev) == 0)
        c->lost_registered = 1;
}

static int win_init(struct win_ctx *c)
{
    AcquireSRWLockExclusive(&c->lock);
    int r = 0;
    if (c->ready && win_lost(c))
        generation_restart(c);
    if (!c->lost_ev) {
        c->lost_ev = CreateEventW(NULL, TRUE /* manual reset */, FALSE, NULL);
        if (!c->lost_ev)
            r = -ENOMEM;
    }
    /* A failed init is NOT latched: the adapter or the KMD may simply not be up
     * yet on the first crm_open, and the next one should get another try. */
    if (!c->ready && r == 0) {
        r = resolve_api(c);
        if (r == 0)
            r = find_adapter(c);
        if (r == 0) {
            /* The first generation attaches at the table's epoch then; a later
             * one was rebased by generation_restart. */
            int32_t table_epoch = c->gen.loss_epoch0;
            if (!c->kmdmap_attached) {
                table_epoch = helios_kmdmap_attach();
                c->kmdmap_attached = 1;
            }
            if (crm_win_gen_accept(&c->gen, c->caps_epoch, table_epoch) != 0) {
                /* probe_adapter already refuses epoch 0; not reached. */
                close_handles(c, c->adapter, c->device, c->context);
                c->adapter = c->device = c->context = 0;
                r = -ENODEV;
            }
        }
        if (r == 0) {
            c->ready = 1;
            register_lost_event(c);
            read_host_tables(c);
        }
    }
    ReleaseSRWLockExclusive(&c->lock);
    return r;
}

/* ---- transport callbacks --------------------------------------------------- */

#define REPLY_SLACK 64u /* room past a bare-header reply; the KMD needs > 16 */

/* MsgHeader.status of a reply: 0, or a negative errno. */
static int reply_status(const uint8_t *resp, uint32_t n)
{
    return n >= CRM_WIRE_HDR ? (int32_t)crm_get32(resp + 8) : -EIO;
}

static int win_open_once(struct win_ctx *c, int32_t node, int *fd)
{
    *fd = -1;
    int r = win_init(c);
    if (r)
        return r;
    if (node != CRM_NODE_CTL && (node < 0 || node > 254))
        return -ENODEV;

    uint8_t req[CRM_WIRE_HDR + 8];
    uint8_t resp[CRM_WIRE_HDR + REPLY_SLACK];
    uint32_t n = 0;
    const size_t req_len =
        crm_wire_open(req, node == CRM_NODE_CTL ? CRM_WIRE_DEV_CTL : (uint32_t)node);
    r = win_forward(c, req, (uint32_t)req_len, resp, sizeof(resp), &n, 0, 0);
    if (r)
        return r;
    r = reply_status(resp, n);
    if (r < 0)
        return r;
    const uint32_t handle = crm_get32(resp + 4);
    if (handle == 0 || handle > 0x7fffffffu)
        return -EIO;
    *fd = (int)handle;
    if (node == CRM_NODE_CTL) {
        AcquireSRWLockExclusive(&c->lock);
        c->ctl_handle = (int)handle;
        ReleaseSRWLockExclusive(&c->lock);
    }
    fd_track(c, *fd);
    return 0;
}

/* The first escape after the KMD went away fails with "device gone" and marks
 * the process lost; one retry then runs on a new generation (win_init). */
static int retry_after_loss(struct win_ctx *c, int r)
{
    return r != 0 && c->kmdmap_attached && win_lost(c);
}

static int win_open(void *vctx, int32_t node, int *fd)
{
    struct win_ctx *c = vctx;
    int r = win_open_once(c, node, fd);
    if (retry_after_loss(c, r))
        r = win_open_once(c, node, fd);
    return r;
}

static void win_close_one(struct win_ctx *c, int fd);

/* Close every channel still waiting for its RM_UNMAP_MEMORY. */
static void pend_flush(struct win_ctx *c)
{
    for (;;) {
        int fd = -1;
        AcquireSRWLockExclusive(&c->lock);
        if (c->n_pend)
            fd = c->pend[--c->n_pend].fd;
        ReleaseSRWLockExclusive(&c->lock);
        if (fd < 0)
            return;
        win_close_one(c, fd);
    }
}

static void win_close(void *vctx, int fd)
{
    struct win_ctx *c = vctx;
    /* A valid fd implies init completed (it came from win_open). */
    if (fd < 0)
        return;
    /* The library releases CPU mappings before closing channels, so by now none of
     * their RM_UNMAP_MEMORY is coming: free those channels first. */
    if (fd == c->ctl_handle) {
        pend_flush(c);
        AcquireSRWLockExclusive(&c->lock);
        c->ctl_handle = -1;
        ReleaseSRWLockExclusive(&c->lock);
    }
    win_close_one(c, fd);
}

/* HELIOS_NVRM_OP_EVENT_REGISTER / UNREGISTER for channel `fd`, kind `kind`. */
static int nvrm_event_call_kind(struct win_ctx *c, uint32_t op, uint32_t fd, uint32_t kind,
                                HANDLE ev)
{
    HeliosNvrmEvent e;
    memset(&e, 0, sizeof(e));
    helios_nvrm_init(&e.head, op, sizeof(e));
    e.handle = fd;
    e.kind = kind;
    e.event_handle = (uint64_t)(uintptr_t)ev;
    int r = nvrm_escape(c, &e, sizeof(e));
    return r ? r : kmd_status_to_errno(e.head.status);
}

/* ... kind READY, the channel events of event_wait. */
static int nvrm_event_call(struct win_ctx *c, uint32_t op, uint32_t fd, HANDLE ev)
{
    return nvrm_event_call_kind(c, op, fd, HELIOS_NVRM_EVENT_READY, ev);
}

/* Forget channel `fd`'s event (before the channel itself closes). */
static void ev_drop(struct win_ctx *c, int fd)
{
    HANDLE ev = NULL;
    AcquireSRWLockExclusive(&c->lock);
    for (uint32_t i = 0; i < c->n_evs; i++) {
        if (c->evs[i].fd == fd) {
            ev = c->evs[i].ev;
            c->evs[i] = c->evs[--c->n_evs];
            break;
        }
    }
    ReleaseSRWLockExclusive(&c->lock);
    if (ev) {
        (void)nvrm_event_call(c, HELIOS_NVRM_OP_EVENT_UNREGISTER, (uint32_t)fd, ev);
        CloseHandle(ev);
    }
}

static void win_close_one(struct win_ctx *c, int fd)
{
    AcquireSRWLockExclusive(&c->lock);
    const int stale = fd_list_remove(c->stale_fds, &c->n_stale, fd);
    if (!stale)
        (void)fd_list_remove(c->live_fds, &c->n_live, fd);
    ReleaseSRWLockExclusive(&c->lock);
    if (stale)
        return; /* closed on the host at its reset; nothing to send */
    ev_drop(c, fd);
    uint8_t req[CRM_WIRE_HDR];
    uint8_t resp[CRM_WIRE_HDR + REPLY_SLACK];
    uint32_t n = 0;
    const size_t req_len = crm_wire_close(req, (uint32_t)fd);
    (void)win_forward(c, req, (uint32_t)req_len, resp, sizeof(resp), &n, 0, 0);
}

static uint32_t host_alloc_param_size(const struct win_ctx *c, uint32_t cls)
{
    for (uint32_t i = 0; i < c->n_alloc; i++)
        if (c->alloc_pairs[2 * i] == cls)
            return c->alloc_pairs[2 * i + 1];
    return 0;
}

/* One Ioctl message: `arg` (size bytes) plus an optional nested block that the
 * payload's own pointer addresses. The reply's blocks are copied back to the
 * same two places. Returns 0, or the host's negative errno. */
static int ioctl_wire_cmd(struct win_ctx *c, int fd, uint32_t cmd, void *arg, uint32_t size,
                          void *nested, uint32_t nested_len, uint32_t pin_id,
                          uint32_t rm_status_off)
{
    if (fd < 0)
        return -EBADF;
    if (fd_stale(c, fd))
        return -ENODEV;
    if (size > CRM_WIRE_BLOCK_MAX || nested_len > CRM_WIRE_BLOCK_MAX)
        return -EINVAL;

    const size_t req_len = crm_wire_ioctl_req_size(size, nested_len);
    const size_t resp_cap = crm_wire_ioctl_resp_max(size, nested_len) + REPLY_SLACK;
    uint8_t *req = malloc(req_len);
    uint8_t *resp = malloc(resp_cap);
    int ret;
    if (!req || !resp) {
        ret = -ENOMEM;
        goto out;
    }
    (void)crm_wire_ioctl(req, (uint32_t)fd, cmd, arg, size, nested, nested_len);

    uint32_t n = 0;
    ret = win_forward(c, req, (uint32_t)req_len, resp, (uint32_t)resp_cap, &n, pin_id,
                      rm_status_off);
    if (ret)
        goto out;

    struct crm_wire_reply rep;
    if (crm_wire_parse_ioctl_reply(resp, n, size, nested_len, &rep) != 0) {
        ret = -EIO;
        goto out;
    }
    if (rep.data_len)
        memcpy(arg, rep.data, rep.data_len);
    if (nested && rep.nested_len)
        memcpy(nested, rep.nested, rep.nested_len);
    ret = rep.status;
out:
    free(req);
    free(resp);
    return ret;
}

/* An NVIDIA escape: _IOWR('F', nr, size). */
static int ioctl_wire(struct win_ctx *c, int fd, uint32_t nr, void *arg, uint32_t size,
                      void *nested, uint32_t nested_len, uint32_t pin_id,
                      uint32_t rm_status_off)
{
    return ioctl_wire_cmd(c, fd, crm_wire_cmd(nr, size), arg, size, nested, nested_len, pin_id,
                          rm_status_off);
}

/* HELIOS_NVRM_OP_PIN: the KMD locks [va, va + length) of this process and keeps the
 * page-run table; all we get back is an id for the registration's FORWARD. */
static int nvrm_pin_call(struct win_ctx *c, uint32_t fd, const void *va, uint64_t length,
                         uint32_t h_root, uint32_t h_object, uint32_t *pin_id)
{
    HeliosNvrmPin p;
    memset(&p, 0, sizeof(p));
    helios_nvrm_init(&p.head, HELIOS_NVRM_OP_PIN, sizeof(p));
    p.handle = fd;
    p.user_va = (uint64_t)(uintptr_t)va;
    p.length = length;
    p.h_root = h_root;
    p.h_object = h_object;
    int r = nvrm_escape(c, &p, sizeof(p));
    if (r)
        return r;
    if (p.head.status != HELIOS_NVRM_ST_OK)
        return p.head.status == HELIOS_NVRM_ST_TOO_SCATTERED ? -ENOMEM
                                                              : kmd_status_to_errno(p.head.status);
    if (p.out_pin_id == 0)
        return -EIO;
    *pin_id = p.out_pin_id;
    return 0;
}

/* Release a pin no registration used (a failure path); a used pin is the KMD's. */
static void nvrm_unpin_call(struct win_ctx *c, uint32_t pin_id)
{
    HeliosNvrmUnpin u;
    memset(&u, 0, sizeof(u));
    helios_nvrm_init(&u.head, HELIOS_NVRM_OP_UNPIN, sizeof(u));
    u.pin_id = pin_id;
    (void)nvrm_escape(c, &u, sizeof(u));
}

/* Park `fd` until the RM_UNMAP_MEMORY of (memory, cookie). 0, or -ENOMEM. */
static int pend_add(struct win_ctx *c, uint32_t memory, uint64_t cookie, int fd)
{
    int r = 0;
    AcquireSRWLockExclusive(&c->lock);
    if (c->n_pend == c->cap_pend) {
        uint32_t ncap = c->cap_pend ? c->cap_pend * 2 : 8;
        struct win_pend *n = realloc(c->pend, (size_t)ncap * sizeof(*n));
        if (!n) {
            r = -ENOMEM;
        } else {
            c->pend = n;
            c->cap_pend = ncap;
        }
    }
    if (r == 0)
        c->pend[c->n_pend++] = (struct win_pend){ .memory = memory, .cookie = cookie, .fd = fd };
    ReleaseSRWLockExclusive(&c->lock);
    return r;
}

/* The parked channel of (memory, cookie), removed; -1 if none. */
static int pend_take(struct win_ctx *c, uint32_t memory, uint64_t cookie)
{
    int fd = -1;
    AcquireSRWLockExclusive(&c->lock);
    for (uint32_t i = 0; i < c->n_pend; i++) {
        if (c->pend[i].memory == memory && c->pend[i].cookie == cookie) {
            fd = c->pend[i].fd;
            c->pend[i] = c->pend[--c->n_pend];
            break;
        }
    }
    ReleaseSRWLockExclusive(&c->lock);
    return fd;
}

/* NV_ESC_RM_ALLOC_MEMORY of an OS descriptor: pMemory names pages of this process
 * that the GPU will read and write. Pin them in the KMD, then send the allocation
 * with the pin's page-run table in place of the pointer. Any other class goes
 * through unchanged. */
static int alloc_memory_pinned(struct win_ctx *c, int fd, uint32_t nr, void *arg, uint32_t size)
{
    if (size < sizeof(NVOS02_PARAMETERS))
        return -EINVAL;
    NVOS02_PARAMETERS *p = arg;
    if (p->hClass != NV01_MEMORY_SYSTEM_OS_DESCRIPTOR || p->pMemory == 0)
        return ioctl_wire(c, fd, nr, arg, size, NULL, 0, 0, 0);

    const uint64_t length = p->limit + 1;
    if (length == 0 || (p->pMemory & 4095u) || (length & 4095u))
        return -EINVAL;
    uint32_t pin_id = 0;
    int r = nvrm_pin_call(c, (uint32_t)fd, (const void *)(uintptr_t)p->pMemory, length,
                          p->hRoot, p->hObjectNew, &pin_id);
    if (r)
        return r;
    /* The RM status of the registration is the block's `status`; the KMD keeps the
     * pin only if it and the host's status are both 0. */
    r = ioctl_wire(c, fd, nr, arg, size, NULL, 0, pin_id,
                   (uint32_t)offsetof(NVOS02_PARAMETERS, status));
    if (r)
        nvrm_unpin_call(c, pin_id); /* never reached RM: PIN_IN_USE if it did, harmless */
    return r;
}

static int win_ioctl(void *vctx, int fd, uint32_t nr, void *arg, uint32_t size)
{
    struct win_ctx *c = vctx;
    void *nested = NULL;
    uint32_t nested_len = 0;

    switch (nr) {
    case NV_ESC_RM_CONTROL:
        /* NVOS54: the parameter block is what `params` addresses. The host splits
         * the message at exactly 32 bytes, so that is the data block whatever the
         * caller passed (as the Linux module sends sizeof(params)). */
        if (size < sizeof(NVOS54_PARAMETERS))
            return -EINVAL;
        {
            const NVOS54_PARAMETERS *p = arg;
            nested = (void *)(uintptr_t)p->params;
            nested_len = p->paramsSize;
            size = (uint32_t)sizeof(NVOS54_PARAMETERS);
        }
        break;
    case NV_ESC_RM_ALLOC:
        /* NVOS64, 48 bytes; the host splits there too, and the Linux module
         * refuses anything smaller. With no explicit size the host's table, read
         * at init, says how big the class's block is. */
        if (size < sizeof(NVOS64_PARAMETERS))
            return -EINVAL;
        {
            const NVOS64_PARAMETERS *p = arg;
            nested = (void *)(uintptr_t)p->pAllocParms;
            nested_len = p->paramsSize;
            if (nested && nested_len == 0)
                nested_len = host_alloc_param_size(c, p->hClass);
            size = (uint32_t)sizeof(NVOS64_PARAMETERS);
        }
        break;
    case NV_ESC_RM_GET_EVENT_DATA:
        /* NVOS41 (16 bytes) plus the one NvUnixEvent that `pEvent` addresses; the
         * host refuses any other split. The event comes back only when RM's status
         * is OK. */
        if (size < sizeof(NVOS41_PARAMETERS))
            return -EINVAL;
        {
            const NVOS41_PARAMETERS *p = arg;
            nested = (void *)(uintptr_t)p->pEvent;
            nested_len = (uint32_t)sizeof(NvUnixEvent);
            size = (uint32_t)sizeof(NVOS41_PARAMETERS);
        }
        break;
    case NV_ESC_RM_ALLOC_MEMORY:
        return alloc_memory_pinned(c, fd, nr, arg, size);
    case NV_ESC_RM_UNMAP_MEMORY:
        if (size >= sizeof(NVOS34_PARAMETERS)) {
            const NVOS34_PARAMETERS *u = arg;
            const uint32_t memory = u->hMemory;
            const uint64_t cookie = u->pLinearAddress;
            const int r = ioctl_wire(c, fd, nr, arg, size, NULL, 0, 0, 0);
            /* RM has dropped (or refused to drop) the mapping: the channel it was
             * armed on can go now. */
            const int held = pend_take(c, memory, cookie);
            if (held >= 0)
                win_close_one(c, held);
            return r;
        }
        break;
    default:
        break;
    }
    if (!nested)
        nested_len = 0;
    return ioctl_wire(c, fd, nr, arg, size, nested, nested_len, 0, 0);
}

/* HELIOS_NVRM_OP_MMAP on backend handle `fd`. */
static int nvrm_mmap_call(struct win_ctx *c, uint32_t fd, uint32_t prot, uint64_t offset,
                          uint64_t size, void **va, uint32_t *id)
{
    HeliosNvrmMmap m;
    memset(&m, 0, sizeof(m));
    helios_nvrm_init(&m.head, HELIOS_NVRM_OP_MMAP, sizeof(m));
    m.handle = fd;
    m.prot = prot;
    m.offset = offset;
    m.size = size;
    m.cache_request = HELIOS_NVRM_CACHE_DEFAULT; /* the KMD picks, as Linux does */
    int r = nvrm_escape(c, &m, sizeof(m));
    if (r)
        return r;
    if (m.head.status != HELIOS_NVRM_ST_OK) {
        /* The host's own refusal comes back as its errno in `flags`. */
        if (m.head.status == HELIOS_NVRM_ST_DEVICE_ERROR && m.flags && m.flags < 4096)
            return -(int)m.flags;
        return kmd_status_to_errno(m.head.status);
    }
    *va = (void *)(uintptr_t)m.out_user_va;
    *id = m.out_mapping_id;
    return 0;
}

static int nvrm_munmap_call(struct win_ctx *c, uint32_t id)
{
    HeliosNvrmMunmap u;
    memset(&u, 0, sizeof(u));
    helios_nvrm_init(&u.head, HELIOS_NVRM_OP_MUNMAP, sizeof(u));
    u.mapping_id = id;
    int r = nvrm_escape(c, &u, sizeof(u));
    return r ? r : kmd_status_to_errno(u.head.status);
}

static int map_table_add(struct win_ctx *c, void *ptr, void *base, int fd, uint32_t id)
{
    int r = 0;
    AcquireSRWLockExclusive(&c->lock);
    if (c->n_maps == c->cap_maps) {
        const uint32_t ncap = c->cap_maps ? c->cap_maps * 2 : 16;
        struct win_map *n = realloc(c->maps, (size_t)ncap * sizeof(*n));
        if (!n) {
            r = -ENOMEM;
        } else {
            c->maps = n;
            c->cap_maps = ncap;
        }
    }
    if (r == 0)
        c->maps[c->n_maps++] =
            (struct win_map){ .ptr = ptr, .base = base, .fd = fd, .id = id, .gen = c->gen.generation };
    ReleaseSRWLockExclusive(&c->lock);
    return r;
}

/* Remove the mapping whose address is `ptr`; 0 and its record, or -ENOENT. */
static int map_table_take(struct win_ctx *c, void *ptr, struct win_map *out)
{
    int r = -ENOENT;
    AcquireSRWLockExclusive(&c->lock);
    for (uint32_t i = 0; i < c->n_maps; i++) {
        if (c->maps[i].ptr == ptr) {
            *out = c->maps[i];
            c->maps[i] = c->maps[--c->n_maps];
            r = 0;
            break;
        }
    }
    ReleaseSRWLockExclusive(&c->lock);
    return r;
}

static int win_map_memory(void *vctx, int ctl_fd, const struct crm_map_request *req, void **cpu_ptr,
                          uint64_t *cookie)
{
    struct win_ctx *c = vctx;
    *cpu_ptr = NULL;
    *cookie = 0;
    if (!c->ready || fd_stale(c, ctl_fd))
        return -ENODEV;

    const uint64_t pm = 4096 - 1;
    const uint64_t start = req->offset & ~pm;
    const uint64_t end = (req->offset + req->length + pm) & ~pm;
    if (end <= start)
        return -EINVAL;
    uint32_t prot = 0;
    switch (req->flags & NVOS33_FLAGS_ACCESS_MASK) {
    case NVOS33_FLAGS_ACCESS_READ_ONLY:
        prot = CRM_PROT_READ;
        break;
    case NVOS33_FLAGS_ACCESS_WRITE_ONLY:
        prot = CRM_PROT_WRITE;
        break;
    default:
        prot = CRM_PROT_READ | CRM_PROT_WRITE;
        break;
    }

    /* The library chose the channel kind that most likely suits the memory
     * (system memory maps on the control channel, BAR memory on the GPU's). If RM
     * says that was the wrong kind, a GPU-minor hint can fall back to the control
     * channel; the reverse needs a minor the transport does not know. */
    int32_t nodes[2] = { req->node_hint, CRM_NODE_CTL };
    const int attempts = req->node_hint == CRM_NODE_CTL ? 1 : 2;
    int r = -EINVAL;
    for (int a = 0; a < attempts; a++) {
        int fd = -1;
        r = win_open(c, nodes[a], &fd);
        if (r < 0)
            return r;
        if (nodes[a] != CRM_NODE_CTL) {
            nv_ioctl_register_fd_t reg = { .ctl_fd = ctl_fd };
            (void)win_ioctl(c, fd, NV_ESC_REGISTER_FD, &reg, sizeof(reg));
        }
        nv_ioctl_nvos33_parameters_with_fd p;
        memset(&p, 0, sizeof(p));
        p.params.hClient = req->h_client;
        p.params.hDevice = req->h_device;
        p.params.hMemory = req->h_memory;
        p.params.offset = req->offset;
        p.params.length = req->length;
        p.params.flags = req->flags;
        p.fd = fd;
        r = win_ioctl(c, ctl_fd, NV_ESC_RM_MAP_MEMORY, &p, sizeof(p));
        if (r == 0 && p.params.status != 0)
            r = (int)p.params.status;
        if (r != 0) {
            win_close(c, fd);
            /* NV_ERR_INVALID_ARGUMENT: wrong channel kind, RM already undid its
             * side. */
            if (r == (int)NV_ERR_INVALID_ARGUMENT && a + 1 < attempts)
                continue;
            return r;
        }

        void *base = NULL;
        uint32_t id = 0;
        r = nvrm_mmap_call(c, (uint32_t)fd, prot, 0, end - start, &base, &id);
        if (r == 0)
            r = map_table_add(c, (uint8_t *)base + (req->offset - start), base, fd, id);
        if (r == 0)
            helios_kmdmap_register(base, end - start, (uint64_t)(uintptr_t)c);
        if (r != 0) {
            if (base && id)
                (void)nvrm_munmap_call(c, id);
            win_close(c, fd);
            NVOS34_PARAMETERS u;
            memset(&u, 0, sizeof(u));
            u.hClient = req->h_client;
            u.hDevice = req->h_device;
            u.hMemory = req->h_memory;
            u.pLinearAddress = p.params.pLinearAddress;
            u.flags = req->flags;
            (void)win_ioctl(c, ctl_fd, NV_ESC_RM_UNMAP_MEMORY, &u, sizeof(u));
            return r;
        }
        *cpu_ptr = (uint8_t *)base + (req->offset - start);
        *cookie = p.params.pLinearAddress;
        return 0;
    }
    return r;
}

static int win_unmap_memory(void *vctx, int ctl_fd, const struct crm_map_request *req, void *cpu_ptr,
                            uint64_t cookie)
{
    struct win_ctx *c = vctx;
    (void)ctl_fd;
    struct win_map m;
    if (map_table_take(c, cpu_ptr, &m) != 0)
        /* Not ours, or a view of an earlier generation that generation_restart
         * dropped: nothing to send (the KMD and host that made it are gone). */
        return c->gen.generation > 1 ? 0 : -ENOENT;
    /* The view goes first, then the host's mapping. The channel RM armed the
     * mapping on stays open until the library's NV_ESC_RM_UNMAP_MEMORY (win_ioctl
     * closes it then), or until the control channel closes. Out of the loss
     * table before the KMD unmaps the view. */
    helios_kmdmap_unregister(m.base);
    (void)nvrm_munmap_call(c, m.id);
    if (pend_add(c, req->h_memory, cookie, m.fd) != 0)
        win_close(c, m.fd);
    return 0;
}

/* The event of channel `fd`, created and registered with the KMD on first use. */
static int ev_get(struct win_ctx *c, int fd, HANDLE *out)
{
    int r = 0;
    AcquireSRWLockExclusive(&c->lock);
    for (uint32_t i = 0; i < c->n_evs; i++) {
        if (c->evs[i].fd == fd) {
            *out = c->evs[i].ev;
            ReleaseSRWLockExclusive(&c->lock);
            return 0;
        }
    }
    if (c->n_evs == c->cap_evs) {
        uint32_t ncap = c->cap_evs ? c->cap_evs * 2 : 4;
        struct win_ev *n = realloc(c->evs, (size_t)ncap * sizeof(*n));
        if (!n)
            r = -ENOMEM;
        else {
            c->evs = n;
            c->cap_evs = ncap;
        }
    }
    if (r == 0) {
        HANDLE ev = CreateEventW(NULL, TRUE /* manual reset */, FALSE, NULL);
        if (!ev) {
            r = -ENOMEM;
        } else {
            /* The KMD latches a wake that arrived before the registration and
             * signals it at once, so nothing is lost here. */
            r = nvrm_event_call(c, HELIOS_NVRM_OP_EVENT_REGISTER, (uint32_t)fd, ev);
            if (r == 0) {
                c->evs[c->n_evs++] = (struct win_ev){ .fd = fd, .ev = ev };
                *out = ev;
            } else {
                CloseHandle(ev);
            }
        }
    }
    ReleaseSRWLockExclusive(&c->lock);
    return r;
}

/* One QUERY_CAPS round trip, judged like any reply: marks the loss when the
 * transport answering is not this generation's. */
static void probe_transport(struct win_ctx *c)
{
    HeliosNvrmQueryCaps caps;
    memset(&caps, 0, sizeof(caps));
    helios_nvrm_init(&caps.head, HELIOS_NVRM_OP_QUERY_CAPS, sizeof(caps));
    (void)nvrm_escape(c, &caps, sizeof(caps));
}

/* Block until the host reports channel `fd` readable. The event is level style:
 * a wake that arrives while nobody waits stays signalled. Reset it on the way out
 * (before the caller drains), so a wake after the reset is seen by the next wait. */
static int win_event_wait(void *vctx, int fd, uint32_t timeout_ms)
{
    struct win_ctx *c = vctx;
    if (fd < 0)
        return -EBADF;
    if (fd_stale(c, fd))
        return -ENODEV;
    HANDLE ev = NULL;
    int r = ev_get(c, fd, &ev);
    if (r)
        return r;
    const DWORD ms = timeout_ms == 0xFFFFFFFFu ? INFINITE : (DWORD)timeout_ms;
    LARGE_INTEGER t0, t1;
    if (g_prof_on > 0)
        QueryPerformanceCounter(&t0);
    const ULONGLONG deadline = ms == INFINITE ? 0 : GetTickCount64() + ms;
    /* Short timeouts end on a high-resolution waitable timer: a wait's own
     * timeout, like Sleep, only expires on a timer tick (~15.6 ms at the
     * default resolution), so a 1 ms poll would take a whole tick.  A wake
     * that is lost (the event is shared by every waiter of the process and
     * reset by whichever wakes first) then costs about the timeout, not a
     * tick.  Per thread, created once (Windows 10 1803+; else the old way).
     */
#if defined(_MSC_VER)
#define CRM_THREAD_LOCAL __declspec(thread)
#else
#define CRM_THREAD_LOCAL __thread
#endif
    static CRM_THREAD_LOCAL HANDLE hires_timer;
    static CRM_THREAD_LOCAL int hires_tried;
    if (!hires_tried) {
        hires_tried = 1;
        hires_timer = CreateWaitableTimerExW(NULL, NULL,
                                             0x00000002 /* CREATE_WAITABLE_TIMER_HIGH_RESOLUTION */,
                                             TIMER_ALL_ACCESS);
    }
    const int use_timer = hires_timer != NULL && ms != INFINITE && ms <= 50;
    if (use_timer) {
        LARGE_INTEGER due;
        due.QuadPart = -(LONGLONG)ms * 10000; /* 100 ns units, relative */
        if (!SetWaitableTimer(hires_timer, &due, 0, NULL, NULL, FALSE))
            return 0;
    }
    for (;;) {
        DWORD left = ms;
        if (ms != INFINITE) {
            const ULONGLONG now = GetTickCount64();
            left = now >= deadline ? 0 : (DWORD)(deadline - now);
        }
        HANDLE waits[3] = { ev, c->lost_ev, NULL };
        DWORD nwaits = c->lost_ev ? 2 : 1;
        if (use_timer) {
            waits[nwaits] = hires_timer;
            left = INFINITE;
        }
        const DWORD timer_idx = WAIT_OBJECT_0 + nwaits;
        if (use_timer)
            nwaits++;
        DWORD w = WaitForMultipleObjects(nwaits, waits, FALSE, left);
        if (use_timer && w == timer_idx)
            w = WAIT_TIMEOUT;
        if (g_prof_on > 0) {
            /* evwait 0x0 = woken by the event, 0x1 = timed out */
            QueryPerformanceCounter(&t1);
            prof_record(((uint64_t)PROF_WAIT << 32) | (w == WAIT_OBJECT_0 ? 0u : 1u), t0, t1);
        }
        switch (w) {
        case WAIT_OBJECT_0:
            ResetEvent(ev);
            /* The KMD signals every registration when it resets the device. */
            return win_lost(c) || fd_stale(c, fd) ? -ENODEV : 1;
        case WAIT_OBJECT_0 + 1:
            /* The loss event: set by win_mark_lost, or by the KMD. A late signal
             * from a KMD that is already gone can arrive after generation_restart
             * re-armed it, so ask the transport before believing it: QUERY_CAPS's
             * reply is judged like every other (a changed epoch marks the loss). */
            if (!win_lost(c) && !fd_stale(c, fd))
                probe_transport(c);
            if (win_lost(c) || fd_stale(c, fd))
                return -ENODEV;
            ResetEvent(c->lost_ev); /* stale wake: this generation is alive */
            continue;
        case WAIT_TIMEOUT:
            return 0;
        default:
            return -EIO;
        }
    }
}

static int win_alloc_pages(void *ctx, uint64_t size, void **ptr)
{
    (void)ctx;
    if ((uint64_t)(SIZE_T)size != size)
        return -ENOMEM;
    /* Committed pages are zero filled; MEM_COMMIT makes them count against the
     * commit limit now rather than fault later. */
    void *p = VirtualAlloc(NULL, (SIZE_T)size, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE);
    if (!p)
        return -ENOMEM;
    *ptr = p;
    return 0;
}

static void win_free_pages(void *ctx, void *ptr, uint64_t size)
{
    (void)ctx;
    (void)size;
    VirtualFree(ptr, 0, MEM_RELEASE);
}

/* ---- extras: DRM node, raw ioctl, ScanoutFlip (rmclient_transport.h) ----- */

static int open_device_once(struct win_ctx *c, uint32_t device_type, int *fd)
{
    *fd = -1;
    int r = win_init(c);
    if (r)
        return r;
    uint8_t req[CRM_WIRE_HDR + 8];
    uint8_t resp[CRM_WIRE_HDR + REPLY_SLACK];
    uint32_t n = 0;
    const size_t req_len = crm_wire_open(req, device_type);
    r = win_forward(c, req, (uint32_t)req_len, resp, sizeof(resp), &n, 0, 0);
    if (r)
        return r;
    r = reply_status(resp, n);
    if (r < 0)
        return r;
    const uint32_t handle = crm_get32(resp + 4);
    if (handle == 0 || handle > 0x7fffffffu)
        return -EIO;
    *fd = (int)handle;
    fd_track(c, *fd);
    return 0;
}

int crm_win_open_device(uint32_t device_type, int *fd)
{
    struct win_ctx *c = &g_ctx;
    int r = open_device_once(c, device_type, fd);
    if (retry_after_loss(c, r))
        r = open_device_once(c, device_type, fd);
    return r;
}

void crm_win_close_device(int fd)
{
    if (fd >= 0 && g_ctx.ready)
        win_close_one(&g_ctx, fd);
}

int crm_win_ioctl(int fd, uint32_t cmd, void *arg, uint32_t size, void *nested,
                  uint32_t nested_len)
{
    if (!g_ctx.ready)
        return -ENODEV;
    if (!nested)
        nested_len = 0;
    return ioctl_wire_cmd(&g_ctx, fd, cmd, arg, size, nested, nested_len, 0, 0);
}

int crm_win_scanout_flip(const struct crm_scanout_flip *flip)
{
    struct win_ctx *c = &g_ctx;
    if (!c->ready)
        return -ENODEV;
    const struct crm_wire_flip f = {
        .scanout = flip->scanout,
        .owner_handle = flip->owner_handle,
        .host_handle = flip->host_handle,
        .width = flip->width,
        .height = flip->height,
        .stride = flip->stride,
        .offset = flip->offset,
        .fourcc = flip->fourcc,
        .modifier = flip->modifier,
        .seq = flip->seq,
    };
    uint8_t req[CRM_WIRE_HDR + CRM_WIRE_SCANOUT_FLIP];
    uint8_t resp[CRM_WIRE_HDR + REPLY_SLACK];
    uint32_t n = 0;
    const size_t req_len = crm_wire_scanout_flip(req, &f);
    const int r = win_forward(c, req, (uint32_t)req_len, resp, sizeof(resp), &n, 0, 0);
    return r ? r : reply_status(resp, n);
}

/* Foreign scanout source (KMD 22.22.308+, QUERY_CAPS bits 9..11): the KMD owns
 * the ScanoutFlip, mints its seq and keeps the desktop's own flips off scanout 0
 * while the source is live. -ENOSYS from a KMD without the ops: send ScanoutFlip
 * with crm_win_scanout_flip instead. */
static int scanout_op_ready(struct win_ctx *c, uint32_t op)
{
    if (!c->ready)
        return -ENODEV;
    return (c->supported_ops & (1ull << op)) ? 0 : -ENOSYS;
}

int crm_win_scanout_set(struct crm_scanout_source *src)
{
    struct win_ctx *c = &g_ctx;
    if (fd_stale(c, (int)src->handle))
        return -ENODEV;
    int r = scanout_op_ready(c, HELIOS_NVRM_OP_SCANOUT_SET);
    if (r)
        return r;
    HeliosNvrmScanoutSet s;
    memset(&s, 0, sizeof(s));
    helios_nvrm_init(&s.head, HELIOS_NVRM_OP_SCANOUT_SET, sizeof(s));
    s.handle = src->handle;
    s.width = src->width;
    s.height = src->height;
    s.stride = src->stride;
    s.offset = src->offset;
    s.fourcc = src->fourcc;
    s.lapse_ms = src->lapse_ms;
    s.modifier = src->modifier;
    r = nvrm_escape(c, &s, sizeof(s));
    if (r)
        return r;
    if (s.head.status != HELIOS_NVRM_ST_OK)
        return kmd_status_to_errno(s.head.status);
    if (s.out_generation == 0)
        return -EIO;
    src->lapse_ms = s.lapse_ms;
    src->generation = s.out_generation;
    return 0;
}

int crm_win_scanout_present(uint32_t handle, uint32_t gem, uint64_t *seq)
{
    struct win_ctx *c = &g_ctx;
    if (fd_stale(c, (int)handle))
        return -ENODEV;
    int r = scanout_op_ready(c, HELIOS_NVRM_OP_SCANOUT_PRESENT);
    if (r)
        return r;
    HeliosNvrmScanoutPresent p;
    memset(&p, 0, sizeof(p));
    helios_nvrm_init(&p.head, HELIOS_NVRM_OP_SCANOUT_PRESENT, sizeof(p));
    p.handle = handle;
    p.gem = gem;
    r = nvrm_escape(c, &p, sizeof(p));
    if (r)
        return r;
    if (p.head.status != HELIOS_NVRM_ST_OK)
        return kmd_status_to_errno(p.head.status);
    if (seq)
        *seq = p.out_seq;
    return 0;
}

int crm_win_scanout_release(uint32_t handle)
{
    struct win_ctx *c = &g_ctx;
    if (fd_stale(c, (int)handle))
        return -ENODEV;
    int r = scanout_op_ready(c, HELIOS_NVRM_OP_SCANOUT_RELEASE);
    if (r)
        return r;
    HeliosNvrmScanoutRelease rel;
    memset(&rel, 0, sizeof(rel));
    helios_nvrm_init(&rel.head, HELIOS_NVRM_OP_SCANOUT_RELEASE, sizeof(rel));
    rel.handle = handle;
    r = nvrm_escape(c, &rel, sizeof(rel));
    if (r)
        return r;
    return kmd_status_to_errno(rel.head.status);
}

/* ---- RM fences (guest/windows/docs/rm-fence-marker.md, docs/SYNC.md) ------
 * nvidia-drm's semaphore-surface fences on a host render node: a fence context
 * imports an RM NV_SEMAPHORE_SURFACE of one of this guest's RM clients, a fence
 * is a backend handle that fires one EventReady when the surface's slot reaches
 * a value (or after nvidia-drm's timeout). The KMD records the handle a
 * forwarded SEMSURF_FENCE_CREATE returns as this device's (KMD 22.22.311+), so
 * EVENT_REGISTER and Close work on it, and SCANOUT_PRESENT can take it over. */

#define CRM_DRM_IOWR(nr, size) ((3u << 30) | ((uint32_t)(size) << 16) | ('d' << 8) | (nr))
#define CRM_SEMSURF_FENCE_CTX_CREATE CRM_DRM_IOWR(0x54, 32)
#define CRM_SEMSURF_FENCE_CREATE CRM_DRM_IOWR(0x55, 24)
#define CRM_NVGPU_CFG_DRM_FENCES (1u << 11)

int crm_win_caps(uint64_t *supported_ops, uint32_t *device_features)
{
    struct win_ctx *c = &g_ctx;
    int r = win_init(c);
    if (r)
        return r;
    if (supported_ops)
        *supported_ops = c->supported_ops;
    if (device_features)
        *device_features = c->device_features;
    return 0;
}

int crm_win_semsurf_ctx_create(int drm_fd, uint32_t h_client, uint32_t h_semsurf,
                               uint64_t size, uint64_t index, uint32_t *ctx)
{
    struct win_ctx *c = &g_ctx;
    *ctx = 0;
    if (!c->ready)
        return -ENODEV;
    if (!(c->device_features & CRM_NVGPU_CFG_DRM_FENCES))
        return -ENOSYS;
    struct {
        uint64_t index;
        uint64_t nvkms_params_ptr;
        uint64_t nvkms_params_size;
        uint32_t handle;
        uint32_t pad;
    } p;
    /* NvKmsKapiPrivImportSemaphoreSurfaceParams */
    struct {
        uint32_t h_client;
        uint32_t h_semaphore_surface;
        uint64_t size;
    } nvkms = { h_client, h_semsurf, size };
    memset(&p, 0, sizeof(p));
    p.index = index;
    p.nvkms_params_ptr = (uint64_t)(uintptr_t)&nvkms;
    p.nvkms_params_size = sizeof(nvkms);
    int r = ioctl_wire_cmd(c, drm_fd, CRM_SEMSURF_FENCE_CTX_CREATE, &p, sizeof(p), &nvkms,
                           sizeof(nvkms), 0, 0);
    if (r)
        return r;
    if (p.handle == 0)
        return -EIO;
    *ctx = p.handle;
    return 0;
}

int crm_win_semsurf_fence_create(int drm_fd, uint32_t ctx, uint64_t wait_value,
                                 uint32_t timeout_ms, int *fence)
{
    struct win_ctx *c = &g_ctx;
    *fence = -1;
    if (!c->ready)
        return -ENODEV;
    struct {
        uint32_t ctx;
        uint32_t timeout_ms;
        uint64_t wait_value;
        int32_t fd;
        uint32_t pad;
    } p = { ctx, timeout_ms, wait_value, -1, 0 };
    int r = ioctl_wire_cmd(c, drm_fd, CRM_SEMSURF_FENCE_CREATE, &p, sizeof(p), NULL, 0, 0, 0);
    if (r)
        return r;
    if (p.fd <= 0)
        return -EIO;
    *fence = p.fd;
    return 0;
}

int crm_win_fence_wait(int fence, uint32_t timeout_ms)
{
    if (!g_ctx.ready)
        return -ENODEV;
    return win_event_wait(&g_ctx, fence, timeout_ms);
}

int crm_win_scanout_present_fenced(uint32_t handle, uint32_t gem, int fence, uint64_t *seq)
{
    struct win_ctx *c = &g_ctx;
    if (fd_stale(c, (int)handle))
        return -ENODEV;
    int r = scanout_op_ready(c, HELIOS_NVRM_OP_SCANOUT_PRESENT);
    if (r)
        return r;
    if (!(c->supported_ops & HELIOS_NVRM_CAP_SCANOUT_FENCE))
        return -ENOSYS;
    if (fence <= 0)
        return -EINVAL;
    HeliosNvrmScanoutPresent p;
    memset(&p, 0, sizeof(p));
    helios_nvrm_init(&p.head, HELIOS_NVRM_OP_SCANOUT_PRESENT, sizeof(p));
    p.handle = handle;
    p.gem = gem;
    p.flags = HELIOS_NVRM_SCANOUT_PRESENT_FLAG_RM_FENCE;
    p.rm_fence_handle = (uint32_t)fence;
    r = nvrm_escape(c, &p, sizeof(p));
    if (r)
        return r;
    switch (p.head.status) {
    case HELIOS_NVRM_ST_OK:
        break;
    case HELIOS_NVRM_ST_QUEUE_FULL:
        return -EAGAIN;
    case HELIOS_NVRM_ST_FENCE_ATTACHED:
        return -EALREADY;
    default:
        return kmd_status_to_errno(p.head.status);
    }
    if (seq)
        *seq = p.out_seq;
    return 0;
}

/* ---- buffer release (KMD 22.22.315+, guest/windows/docs/foreign-scanout.md
 * "Buffer release"): SCANOUT_STATUS and the SCANOUT_RELEASED event. ---------- */

int crm_win_scanout_status(uint32_t handle, uint64_t *released_seq, uint64_t *last_seq)
{
    struct win_ctx *c = &g_ctx;
    if (fd_stale(c, (int)handle))
        return -ENODEV;
    if (!c->ready)
        return -ENODEV;
    if (!(c->supported_ops & HELIOS_NVRM_CAP_SCANOUT_RELEASE) ||
        !(c->supported_ops & (1ull << HELIOS_NVRM_OP_SCANOUT_STATUS)))
        return -ENOSYS;
    HeliosNvrmScanoutStatus st;
    memset(&st, 0, sizeof(st));
    helios_nvrm_init(&st.head, HELIOS_NVRM_OP_SCANOUT_STATUS, sizeof(st));
    st.handle = handle;
    int r = nvrm_escape(c, &st, sizeof(st));
    if (r)
        return r;
    if (st.head.status != HELIOS_NVRM_ST_OK)
        return kmd_status_to_errno(st.head.status);
    if (released_seq)
        *released_seq = st.out_released_seq;
    if (last_seq)
        *last_seq = st.out_last_seq;
    return 0;
}

/* The process's SCANOUT_RELEASED event, registered on first use. */
static int release_event(struct win_ctx *c, HANDLE *out)
{
    int r = 0;
    AcquireSRWLockExclusive(&c->lock);
    if (!c->release_ev) {
        HANDLE ev = CreateEventW(NULL, FALSE /* auto reset */, FALSE, NULL);
        if (!ev) {
            r = -ENOMEM;
        } else {
            r = nvrm_event_call_kind(c, HELIOS_NVRM_OP_EVENT_REGISTER, 0,
                                     HELIOS_NVRM_EVENT_SCANOUT_RELEASED, ev);
            if (r == 0)
                c->release_ev = ev;
            else
                CloseHandle(ev);
        }
    }
    *out = c->release_ev;
    ReleaseSRWLockExclusive(&c->lock);
    return r;
}

int crm_win_scanout_wait_released(uint32_t handle, uint64_t seq, uint32_t timeout_ms,
                                  uint64_t *released_seq)
{
    struct win_ctx *c = &g_ctx;
    uint64_t released = 0;
    int r = crm_win_scanout_status(handle, &released, NULL);
    if (released_seq)
        *released_seq = released;
    if (r)
        return r;
    if (released >= seq)
        return 1;
    if (timeout_ms == 0)
        return 0;

    HANDLE ev = NULL;
    r = release_event(c, &ev);
    if (r)
        return r;
    /* The event is a doorbell, not a latch: reset, ask, and only then wait,
     * so a release between the question and the wait still wakes us. */
    const ULONGLONG deadline = GetTickCount64() + timeout_ms;
    for (;;) {
        ResetEvent(ev);
        r = crm_win_scanout_status(handle, &released, NULL);
        if (released_seq)
            *released_seq = released;
        if (r)
            return r;
        if (released >= seq)
            return 1;
        const ULONGLONG now = GetTickCount64();
        if (now >= deadline)
            return 0;
        const DWORD w = WaitForSingleObject(ev, (DWORD)(deadline - now));
        if (w == WAIT_TIMEOUT) {
            r = crm_win_scanout_status(handle, &released, NULL);
            if (released_seq)
                *released_seq = released;
            if (r)
                return r;
            return released >= seq ? 1 : 0;
        }
        if (w != WAIT_OBJECT_0)
            return -EIO;
    }
}

/* ---- Helios extras beyond NVRM: adapter identity, Venus holder contexts,
 * foreign resources (guest/windows/protocol/src/foreign.rs). All go through the
 * same D3DKMT device as the RM escapes, so the KMD sees one owner for the DRM
 * file, the context and the imported resource. ------------------------------ */

int crm_win_adapter_luid(uint32_t *low, int32_t *high)
{
    struct win_ctx *c = &g_ctx;
    int r = win_init(c);
    if (r)
        return r;
    if (low)
        *low = c->luid.LowPart;
    if (high)
        *high = c->luid.HighPart;
    return 0;
}

/* The HeliosEscapeHeader every non-NVRM verb starts with (protocol escape.rs). */
struct crm_helios_hdr {
    uint32_t magic, cmd_type, version, size;
};
#define CRM_HELIOS_MAGIC 0x48454C53u
#define CRM_HELIOS_ESC_CTX_CREATE 0x0002u
#define CRM_HELIOS_ESC_CTX_DESTROY 0x0003u
#define CRM_HELIOS_ESC_RELEASE_BLOB 0x0008u
#define CRM_HELIOS_ESC_FOREIGN 0x0018u
#define CRM_VIRTIO_GPU_CAPSET_VENUS 4u

static void helios_hdr(struct crm_helios_hdr *h, uint32_t cmd, uint32_t size)
{
    h->magic = CRM_HELIOS_MAGIC;
    h->cmd_type = cmd;
    h->version = 1;
    h->size = size;
}

int crm_win_venus_ctx_create(uint32_t *ctx_id)
{
    struct win_ctx *c = &g_ctx;
    *ctx_id = 0;
    int r = win_init(c);
    if (r)
        return r;
    struct {
        struct crm_helios_hdr hdr;
        uint32_t capset_id, out_ctx_id;
    } q;
    memset(&q, 0, sizeof(q));
    helios_hdr(&q.hdr, CRM_HELIOS_ESC_CTX_CREATE, sizeof(q));
    q.capset_id = CRM_VIRTIO_GPU_CAPSET_VENUS;
    r = nvrm_escape(c, &q, sizeof(q));
    if (r)
        return r;
    if (q.out_ctx_id == 0)
        return -EIO;
    *ctx_id = q.out_ctx_id;
    return 0;
}

void crm_win_venus_ctx_destroy(uint32_t ctx_id)
{
    struct win_ctx *c = &g_ctx;
    if (!c->ready || ctx_id == 0)
        return;
    struct {
        struct crm_helios_hdr hdr;
        uint32_t ctx_id, padding;
    } q;
    memset(&q, 0, sizeof(q));
    helios_hdr(&q.hdr, CRM_HELIOS_ESC_CTX_DESTROY, sizeof(q));
    q.ctx_id = ctx_id;
    (void)nvrm_escape(c, &q, sizeof(q));
}

int crm_win_release_blob(uint32_t ctx_id, uint32_t resource_id)
{
    struct win_ctx *c = &g_ctx;
    if (!c->ready)
        return -ENODEV;
    struct {
        struct crm_helios_hdr hdr;
        uint32_t ctx_id, resource_id, flags, padding;
    } q;
    memset(&q, 0, sizeof(q));
    helios_hdr(&q.hdr, CRM_HELIOS_ESC_RELEASE_BLOB, sizeof(q));
    q.ctx_id = ctx_id;
    q.resource_id = resource_id;
    return nvrm_escape(c, &q, sizeof(q));
}

/* helios_foreign.h, restated (it lives in guest/windows/protocol/include). */
struct crm_foreign_head {
    struct crm_helios_hdr hdr;
    uint32_t abi_version, op;
    int32_t status;
    uint32_t reserved;
    uint64_t epoch;
};
_Static_assert(sizeof(struct crm_foreign_head) == 40, "foreign head");

static int foreign_status(int32_t st)
{
    switch (st) {
    case 0: return 0;
    case 1: return -ENOSYS;  /* UNSUPPORTED: gate closed */
    case 2: return -EBADF;   /* NOT_OWNED */
    case 3: return -ESRCH;   /* BAD_CONTEXT */
    case 4: return -EINVAL;  /* BAD_RANGE */
    case 5: return -ENOSPC;  /* NO_RESOURCES */
    default: return -EIO;    /* DEVICE_ERROR */
    }
}

int crm_win_foreign_caps(uint32_t *caps_flags)
{
    struct win_ctx *c = &g_ctx;
    *caps_flags = 0;
    int r = win_init(c);
    if (r)
        return r;
    struct {
        struct crm_foreign_head head;
        uint64_t supported_ops;
        uint32_t caps_flags, max_per_owner, max_total, reserved0;
        uint64_t max_bytes_per_resource, max_bytes_per_owner;
        uint32_t live_total, live_owner, imported, refused;
    } q;
    _Static_assert(sizeof(q) == 96, "query caps");
    memset(&q, 0, sizeof(q));
    helios_hdr(&q.head.hdr, CRM_HELIOS_ESC_FOREIGN, sizeof(q));
    q.head.abi_version = 1;
    q.head.op = 1; /* QUERY_CAPS */
    r = nvrm_escape(c, &q, sizeof(q));
    if (r)
        return r; /* -ENOSYS: a KMD without the verb */
    r = foreign_status(q.head.status);
    if (r)
        return r;
    c->foreign_ops = q.supported_ops;
    *caps_flags = q.caps_flags;
    return 0;
}

int crm_win_import_rm_planes(const struct crm_foreign_import *in,
                             const struct crm_foreign_plane *plane1,
                             uint32_t *resource_id, uint32_t *host_errno)
{
    struct win_ctx *c = &g_ctx;
    *resource_id = 0;
    if (host_errno)
        *host_errno = 0;
    if (!c->ready)
        return -ENODEV;
    /* helios_foreign_import_rm_planes (protocol/include/helios_foreign.h); the
     * first 104 bytes are helios_foreign_import_rm_layout, all a request without
     * plane 1 sends. */
    struct {
        struct crm_foreign_head head;
        uint32_t ctx_id, rm_handle, gem_handle, flags;
        uint64_t size;
        uint32_t out_resource_id, out_host_errno;
        uint32_t width, height, stride, offset, fourcc, reserved;
        uint64_t modifier;
        uint64_t p1_modifier;
        uint32_t p1_stride, p1_offset;
    } q;
    _Static_assert(sizeof(q) == 120, "import rm + layout + plane 1");
    _Static_assert(offsetof(__typeof__(q), p1_modifier) == 104, "plane 1 tail");
    const uint32_t bytes = plane1 != NULL ? 120u : 104u;
    memset(&q, 0, sizeof(q));
    helios_hdr(&q.head.hdr, CRM_HELIOS_ESC_FOREIGN, bytes);
    q.head.abi_version = 1;
    q.head.op = 2; /* IMPORT_RM */
    q.ctx_id = in->ctx_id;
    q.rm_handle = in->rm_handle;
    q.gem_handle = in->gem_handle;
    q.flags = 1; /* LAYOUT */
    q.size = in->size;
    q.width = in->width;
    q.height = in->height;
    q.stride = in->stride;
    q.offset = in->offset;
    q.fourcc = in->fourcc;
    q.modifier = in->modifier;
    if (plane1 != NULL) {
        q.flags |= 2; /* PLANE1 */
        q.p1_modifier = plane1->modifier;
        q.p1_stride = plane1->stride;
        q.p1_offset = plane1->offset;
    }
    int r = nvrm_escape(c, &q, bytes);
    if (r)
        return r;
    if (host_errno)
        *host_errno = q.out_host_errno;
    r = foreign_status(q.head.status);
    if (r)
        return r;
    if (q.out_resource_id == 0)
        return -EIO;
    *resource_id = q.out_resource_id;
    return 0;
}

int crm_win_import_rm(const struct crm_foreign_import *in, uint32_t *resource_id,
                      uint32_t *host_errno)
{
    return crm_win_import_rm_planes(in, NULL, resource_id, host_errno);
}

/* FOREIGN_RESOURCE RM_RESOURCE_IMPORT (op 3, KMD 22.22.313+ with a host that
 * serves RmResourceImport): resource `resource_id`, which this device imported
 * or this process opened, as a GEM handle of our DRM file `rm_handle`. */
int crm_win_rm_resource_import(uint32_t rm_handle, uint32_t resource_id, uint32_t *gem_handle,
                               uint64_t *size, uint64_t *modifier, uint32_t *flags,
                               uint32_t *host_errno)
{
    struct win_ctx *c = &g_ctx;
    *gem_handle = 0;
    if (host_errno)
        *host_errno = 0;
    if (!c->ready)
        return -ENODEV;
    if (c->foreign_ops == 0) {
        uint32_t caps_flags = 0;
        (void)crm_win_foreign_caps(&caps_flags);
    }
    if (!(c->foreign_ops & (1ull << 3)))
        return -ENOSYS;
    struct {
        struct crm_foreign_head head;
        uint32_t rm_handle, resource_id, flags, out_gem_handle;
        uint64_t out_size, out_modifier;
        uint32_t out_flags, out_host_errno;
    } q;
    _Static_assert(sizeof(q) == 80, "rm resource import");
    memset(&q, 0, sizeof(q));
    helios_hdr(&q.head.hdr, CRM_HELIOS_ESC_FOREIGN, sizeof(q));
    q.head.abi_version = 1;
    q.head.op = 3; /* RM_RESOURCE_IMPORT */
    q.rm_handle = rm_handle;
    q.resource_id = resource_id;
    int r = nvrm_escape(c, &q, sizeof(q));
    if (r)
        return r;
    if (host_errno)
        *host_errno = q.out_host_errno;
    r = foreign_status(q.head.status);
    if (r)
        return r;
    if (q.out_gem_handle == 0)
        return -EIO;
    *gem_handle = q.out_gem_handle;
    if (size)
        *size = q.out_size;
    if (modifier)
        *modifier = q.out_modifier;
    if (flags)
        *flags = q.out_flags & CRM_RM_IMPORT_MODIFIER_VALID;
    return 0;
}

static struct crm_transport windows_transport = {
    .abi = CRM_TRANSPORT_ABI,
    .flags = 0,
    .name = "windows-conduit",
    .ctx = &g_ctx,
    .page_size = 4096,
    .open = win_open,
    .close = win_close,
    .ioctl = win_ioctl,
    .map_memory = win_map_memory,
    .unmap_memory = win_unmap_memory,
    .event_wait = win_event_wait,
    .alloc_pages = win_alloc_pages,
    .free_pages = win_free_pages,
};

int32_t crm_win_loss_epoch(void)
{
    struct helios_kmdmap_table *t = helios_kmdmap_t;
    if (!t)
        return 0;
    /* A "lost" answer also backs views the KMD unmapped since (rate-limited). */
    if (g_ctx.kmdmap_attached)
        (void)helios_kmdmap_lost(g_ctx.gen.loss_epoch0);
    return (int32_t)InterlockedCompareExchange(&t->epoch, 0, 0);
}

/* Signals this process's wait handle for event channel `fd` (registered by a
 * previous crm_event_wait), with no KMD call: a waiter blocked in
 * win_event_wait wakes and re-reads what it waits for. For a store the CPU
 * made (a host semaphore signal), which raises no GPU interrupt and so no
 * host report. The handle is manual reset, so a kick before the waiter
 * blocks is not lost; a spurious one costs the waiter one wake. Returns 0,
 * or -ENOENT when no wait has registered `fd` yet (nobody can be blocked). */
int crm_win_event_kick(int fd)
{
    struct win_ctx *c = &g_ctx;
    int r = -ENOENT;
    AcquireSRWLockShared(&c->lock);
    for (uint32_t i = 0; i < c->n_evs; i++) {
        if (c->evs[i].fd == fd) {
            SetEvent(c->evs[i].ev);
            r = 0;
            break;
        }
    }
    ReleaseSRWLockShared(&c->lock);
    return r;
}

/* The vectored handler lives in this DLL: take it out when the DLL is
 * unloaded while the process goes on (FreeLibrary; at process exit nothing
 * runs any more). The table entries stay: the Venus ICD or the UMD may still
 * hold handlers on the same table, and our views are the KMD's to unmap. */
BOOL WINAPI DllMain(HINSTANCE inst, DWORD reason, LPVOID reserved);
BOOL WINAPI DllMain(HINSTANCE inst, DWORD reason, LPVOID reserved)
{
    (void)inst;
    if (reason == DLL_PROCESS_DETACH && reserved == NULL && g_ctx.kmdmap_attached)
        helios_kmdmap_detach();
    return TRUE;
}

int crm_win_escape_raw(void *buf, uint32_t size, int32_t *ntstatus)
{
    struct win_ctx *c = &g_ctx;
    if (ntstatus)
        *ntstatus = 0;
    if (!c->ready)
        return -ENODEV;
    if (!buf || size < sizeof(struct crm_helios_hdr))
        return -EINVAL;
    return escape_judged(c, buf, size, ntstatus);
}

const struct crm_transport *crm_windows_transport(void)
{
    return &windows_transport;
}

const struct crm_transport *crm_default_transport(void)
{
    return crm_windows_transport();
}

#else /* !_WIN32 */

const struct crm_transport *crm_windows_transport(void) { return NULL; }
int32_t crm_win_loss_epoch(void) { return 0; }

#include <errno.h>

int crm_win_event_kick(int fd) { (void)fd; return -ENOSYS; }

int crm_win_open_device(uint32_t device_type, int *fd)
{
    (void)device_type;
    *fd = -1;
    return -ENOSYS;
}

void crm_win_close_device(int fd) { (void)fd; }

int crm_win_ioctl(int fd, uint32_t cmd, void *arg, uint32_t size, void *nested,
                  uint32_t nested_len)
{
    (void)fd; (void)cmd; (void)arg; (void)size; (void)nested; (void)nested_len;
    return -ENOSYS;
}

int crm_win_scanout_flip(const struct crm_scanout_flip *flip)
{
    (void)flip;
    return -ENOSYS;
}

int crm_win_rm_resource_import(uint32_t rm_handle, uint32_t resource_id, uint32_t *gem_handle,
                               uint64_t *size, uint64_t *modifier, uint32_t *flags,
                               uint32_t *host_errno)
{
    (void)rm_handle; (void)resource_id; (void)size; (void)modifier; (void)flags;
    *gem_handle = 0;
    if (host_errno)
        *host_errno = 0;
    return -ENOSYS;
}

int crm_win_scanout_set(struct crm_scanout_source *src)
{
    (void)src;
    return -ENOSYS;
}

int crm_win_scanout_present(uint32_t handle, uint32_t gem, uint64_t *seq)
{
    (void)handle; (void)gem; (void)seq;
    return -ENOSYS;
}

int crm_win_scanout_release(uint32_t handle)
{
    (void)handle;
    return -ENOSYS;
}

int crm_win_caps(uint64_t *supported_ops, uint32_t *device_features)
{
    if (supported_ops)
        *supported_ops = 0;
    if (device_features)
        *device_features = 0;
    return -ENOSYS;
}

int crm_win_semsurf_ctx_create(int drm_fd, uint32_t h_client, uint32_t h_semsurf,
                               uint64_t size, uint64_t index, uint32_t *ctx)
{
    (void)drm_fd; (void)h_client; (void)h_semsurf; (void)size; (void)index;
    *ctx = 0;
    return -ENOSYS;
}

int crm_win_semsurf_fence_create(int drm_fd, uint32_t ctx, uint64_t wait_value,
                                 uint32_t timeout_ms, int *fence)
{
    (void)drm_fd; (void)ctx; (void)wait_value; (void)timeout_ms;
    *fence = -1;
    return -ENOSYS;
}

int crm_win_fence_wait(int fence, uint32_t timeout_ms)
{
    (void)fence; (void)timeout_ms;
    return -ENOSYS;
}

int crm_win_scanout_present_fenced(uint32_t handle, uint32_t gem, int fence, uint64_t *seq)
{
    (void)handle; (void)gem; (void)fence; (void)seq;
    return -ENOSYS;
}

int crm_win_scanout_status(uint32_t handle, uint64_t *released_seq, uint64_t *last_seq)
{
    (void)handle; (void)released_seq; (void)last_seq;
    return -ENOSYS;
}

int crm_win_scanout_wait_released(uint32_t handle, uint64_t seq, uint32_t timeout_ms,
                                  uint64_t *released_seq)
{
    (void)handle; (void)seq; (void)timeout_ms; (void)released_seq;
    return -ENOSYS;
}

int crm_win_adapter_luid(uint32_t *low, int32_t *high)
{
    (void)low; (void)high;
    return -ENOSYS;
}

int crm_win_venus_ctx_create(uint32_t *ctx_id)
{
    *ctx_id = 0;
    return -ENOSYS;
}

void crm_win_venus_ctx_destroy(uint32_t ctx_id) { (void)ctx_id; }

int crm_win_release_blob(uint32_t ctx_id, uint32_t resource_id)
{
    (void)ctx_id; (void)resource_id;
    return -ENOSYS;
}

int crm_win_foreign_caps(uint32_t *caps_flags)
{
    *caps_flags = 0;
    return -ENOSYS;
}

int crm_win_import_rm_planes(const struct crm_foreign_import *in,
                             const struct crm_foreign_plane *plane1,
                             uint32_t *resource_id, uint32_t *host_errno)
{
    (void)in; (void)plane1;
    *resource_id = 0;
    if (host_errno)
        *host_errno = 0;
    return -ENOSYS;
}

int crm_win_import_rm(const struct crm_foreign_import *in, uint32_t *resource_id,
                      uint32_t *host_errno)
{
    (void)in;
    *resource_id = 0;
    if (host_errno)
        *host_errno = 0;
    return -ENOSYS;
}

int crm_win_escape_raw(void *buf, uint32_t size, int32_t *ntstatus)
{
    (void)buf;
    (void)size;
    if (ntstatus)
        *ntstatus = 0;
    return -ENOSYS;
}

#endif

