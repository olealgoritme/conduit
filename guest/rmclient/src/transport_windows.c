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
 * CPU mapping follows Linux's protocol, which the host's backend ties to a
 * channel: open a fresh channel, NV_ESC_RM_MAP_MEMORY on the control channel
 * naming it, then map that channel at offset 0 for the page-rounded length. The
 * KMD does the last step (MMAP), and the channel is kept until unmap, because RM
 * and the backend refuse a second mapping on it.
 */
#include "rmclient_transport.h"

#if defined(_WIN32)

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
#include "win_wire.h"

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
    uint64_t epoch;      /* the KMD's device generation at init */

    /* Host's per-class allocation parameter sizes (GetSysFiles section 3). */
    uint32_t *alloc_pairs; /* {class, size} * n_alloc */
    uint32_t n_alloc;

    /* Live CPU mappings made by map_memory, found again by pointer at unmap. */
    struct win_map {
        void *ptr;        /* the address handed to the caller */
        int fd;           /* the channel kept for this mapping */
        uint32_t id;      /* the KMD/host mapping id */
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
    default:
        return -EIO;
    }
}

/* One HELIOS_ESCAPE_NVRM call on an already-built buffer. Returns 0 and leaves
 * the KMD's verdict in head->status, or a negative errno for a transport
 * failure (an older KMD without the verb answers STATUS_NOT_IMPLEMENTED). */
static int nvrm_escape(struct win_ctx *c, void *buf, uint32_t size)
{
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
    return st == 0 ? 0 : nt_to_errno(st);
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
        c->max_buffer = caps.max_buffer_bytes;
        c->epoch = caps.head.epoch;
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

static int win_init(struct win_ctx *c)
{
    AcquireSRWLockExclusive(&c->lock);
    int r = 0;
    /* A failed init is NOT latched: the adapter or the KMD may simply not be up
     * yet on the first crm_open, and the next one should get another try. */
    if (!c->ready) {
        r = resolve_api(c);
        if (r == 0)
            r = find_adapter(c);
        if (r == 0) {
            c->ready = 1;
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

static int win_open(void *vctx, int32_t node, int *fd)
{
    struct win_ctx *c = vctx;
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
    return 0;
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

/* HELIOS_NVRM_OP_EVENT_REGISTER / UNREGISTER for channel `fd`, kind READY. */
static int nvrm_event_call(struct win_ctx *c, uint32_t op, uint32_t fd, HANDLE ev)
{
    HeliosNvrmEvent e;
    memset(&e, 0, sizeof(e));
    helios_nvrm_init(&e.head, op, sizeof(e));
    e.handle = fd;
    e.kind = HELIOS_NVRM_EVENT_READY;
    e.event_handle = (uint64_t)(uintptr_t)ev;
    int r = nvrm_escape(c, &e, sizeof(e));
    return r ? r : kmd_status_to_errno(e.head.status);
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
static int ioctl_wire(struct win_ctx *c, int fd, uint32_t nr, void *arg, uint32_t size,
                      void *nested, uint32_t nested_len, uint32_t pin_id,
                      uint32_t rm_status_off)
{
    if (fd < 0)
        return -EBADF;
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
    (void)crm_wire_ioctl(req, (uint32_t)fd, crm_wire_cmd(nr, size), arg, size, nested, nested_len);

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

static int map_table_add(struct win_ctx *c, void *ptr, int fd, uint32_t id)
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
        c->maps[c->n_maps++] = (struct win_map){ .ptr = ptr, .fd = fd, .id = id };
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
    if (!c->ready)
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
            r = map_table_add(c, (uint8_t *)base + (req->offset - start), fd, id);
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
        return -ENOENT;
    /* The view goes first, then the host's mapping. The channel RM armed the
     * mapping on stays open until the library's NV_ESC_RM_UNMAP_MEMORY (win_ioctl
     * closes it then), or until the control channel closes. */
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

/* Block until the host reports channel `fd` readable. The event is level style:
 * a wake that arrives while nobody waits stays signalled. Reset it on the way out
 * (before the caller drains), so a wake after the reset is seen by the next wait. */
static int win_event_wait(void *vctx, int fd, uint32_t timeout_ms)
{
    struct win_ctx *c = vctx;
    if (fd < 0)
        return -EBADF;
    HANDLE ev = NULL;
    int r = ev_get(c, fd, &ev);
    if (r)
        return r;
    const DWORD ms = timeout_ms == 0xFFFFFFFFu ? INFINITE : (DWORD)timeout_ms;
    switch (WaitForSingleObject(ev, ms)) {
    case WAIT_OBJECT_0:
        ResetEvent(ev);
        return 1;
    case WAIT_TIMEOUT:
        return 0;
    default:
        return -EIO;
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

#endif
