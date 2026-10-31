/* Device handles and late IOCTL completions.
 *
 * A handle is tracked (its IOCTLs logged) when it was opened under a device
 * name that looks like NVIDIA/graphics (NtCreateFile/NtOpenFile hooks), or
 * when its first NtDeviceIoControlFile finds such a name by asking the object
 * manager (NtQueryObject). The probe covers handles opened before nvtrace was
 * injected (nvinject -p, DWM), duplicated into the process or inherited.
 * Handles that are not interesting are remembered in a small lock-free
 * negative cache so a busy socket is queried once, and forgotten on NtClose.
 *
 * IOCTLs that return STATUS_PENDING write their output later. They are kept
 * in a table that the aux thread polls (IO_STATUS_BLOCK.Status leaves
 * STATUS_PENDING when the I/O manager completes the request); the output
 * buffer is then copied into an IOCOMPLETE record.
 */
#include "nvtrace.h"
#include <stdio.h>
#include <string.h>

/* ------------------------------------------------------------------ names */

/* \Device\Name or \??\Name (not \??\C:\... or \??\UNC\...) */
static int device_like(const char *n)
{
    if (!strncmp(n, "\\device\\", 8))
        return 1;
    if (!strncmp(n, "\\??\\", 4) || !strncmp(n, "\\\\?\\", 4) || !strncmp(n, "\\\\.\\", 4)) {
        const char *r = n + 4;
        if (r[0] && r[1] == ':')
            return 0;
        if (!strncmp(r, "unc\\", 4))
            return 0;
        return r[0] != 0;
    }
    return 0;
}

/* Only the device component counts: "\device\harddiskvolume3\...\nvidia\x"
 * is a file, not an NVIDIA device. */
int dev_name_interesting(const char *n)
{
    char comp[64];
    if (!device_like(n))
        return 0;
    if (nvt_cfg.devices_all)
        return 1;
    const char *r = !strncmp(n, "\\device\\", 8) ? n + 8 : n + 4;
    size_t i = 0;
    while (r[i] && r[i] != '\\' && i + 1 < sizeof(comp)) {
        comp[i] = r[i];
        i++;
    }
    comp[i] = 0;
    if (!strncmp(comp, "harddisk", 8) || !strncmp(comp, "mup", 3) || !strncmp(comp, "namedpipe", 9) ||
        !strncmp(comp, "afd", 3))
        return 0;
    return strstr(comp, "nv") || strstr(comp, "uvm") || strstr(comp, "dxg") || strstr(comp, "gpu");
}

/* ------------------------------------------------------------------ tracked */

typedef struct tracked {
    HANDLE h;
    char name[64];
} tracked;

#define MAX_TRACKED 256
static tracked tracked_tab[MAX_TRACKED];
static SRWLOCK tracked_lock = SRWLOCK_INIT;
static volatile LONG ntracked;

int dev_tracked(HANDLE h)
{
    if (!ntracked || !h)
        return 0;
    int found = 0;
    AcquireSRWLockShared(&tracked_lock);
    for (int i = 0; i < MAX_TRACKED; i++)
        if (tracked_tab[i].h == h) {
            found = 1;
            break;
        }
    ReleaseSRWLockShared(&tracked_lock);
    return found;
}

void dev_track(HANDLE h, const char *name)
{
    if (!h || h == INVALID_HANDLE_VALUE)
        return;
    AcquireSRWLockExclusive(&tracked_lock);
    int slot = -1;
    for (int i = 0; i < MAX_TRACKED; i++) {
        if (tracked_tab[i].h == h) {
            slot = i;
            break;
        }
        if (slot < 0 && !tracked_tab[i].h)
            slot = i;
    }
    if (slot >= 0) {
        if (!tracked_tab[slot].h)
            InterlockedIncrement(&ntracked);
        tracked_tab[slot].h = h;
        lstrcpynA(tracked_tab[slot].name, name, sizeof(tracked_tab[slot].name));
    }
    ReleaseSRWLockExclusive(&tracked_lock);
}

void dev_untrack(HANDLE h)
{
    AcquireSRWLockExclusive(&tracked_lock);
    for (int i = 0; i < MAX_TRACKED; i++)
        if (tracked_tab[i].h == h) {
            tracked_tab[i].h = NULL;
            InterlockedDecrement(&ntracked);
        }
    ReleaseSRWLockExclusive(&tracked_lock);
}

/* ------------------------------------------------------------------ probe */

#define NEG_SLOTS 4096
static HANDLE volatile neg_cache[NEG_SLOTS];

static size_t neg_slot(HANDLE h)
{
    return ((uintptr_t)h >> 2) & (NEG_SLOTS - 1);
}

void dev_on_close(HANDLE h)
{
    InterlockedCompareExchangePointer((PVOID volatile *)&neg_cache[neg_slot(h)], NULL, h);
}

typedef LONG(NTAPI *NtQueryObject_t)(HANDLE, ULONG, PVOID, ULONG, PULONG);
static NtQueryObject_t p_query_object;

typedef struct nvt_ustr {
    USHORT Length, MaximumLength;
    PWSTR Buffer;
} nvt_ustr;

static void emit_handle(HANDLE h, uint32_t how, const char *name)
{
    nvt_buf b = {0};
    ntr_handle r;
    memset(&r, 0, sizeof(r));
    r.handle = (uintptr_t)h;
    r.how = how;
    nvt_rec_begin(&b, NTR_REC_HANDLE);
    nvt_buf_put(&b, &r, sizeof(r));
    nvt_buf_put(&b, name, strlen(name) + 1);
    nvt_rec_end(&b);
    nvt_buf_free(&b);
}

int dev_probe(HANDLE h)
{
    if (dev_tracked(h))
        return 1;
    if (!nvt_cfg.probe || !p_query_object || !h || h == INVALID_HANDLE_VALUE)
        return 0;
    size_t slot = neg_slot(h);
    if (neg_cache[slot] == h)
        return 0;
    /* ObjectTypeInformation never blocks; only File objects get a name query */
    union {
        nvt_ustr us;
        uint8_t raw[1024];
    } u;
    ULONG ret = 0;
    char name[200];
    int interesting = 0;
    memset(&u, 0, sizeof(u));
    if (p_query_object(h, 2 /* ObjectTypeInformation */, &u, sizeof(u), &ret) >= 0 &&
        u.us.Buffer && u.us.Length == 8 && !memcmp(u.us.Buffer, L"File", 8)) {
        memset(&u, 0, sizeof(u));
        if (p_query_object(h, 1 /* ObjectNameInformation */, &u, sizeof(u) - 2, &ret) >= 0 &&
            u.us.Buffer) {
            size_t n = u.us.Length / 2, i;
            for (i = 0; i < n && i + 1 < sizeof(name); i++) {
                wchar_t c = u.us.Buffer[i];
                if (c >= 'A' && c <= 'Z')
                    c = c - 'A' + 'a';
                name[i] = (c < 128 && c >= 32) ? (char)c : '?';
            }
            name[i] = 0;
            interesting = dev_name_interesting(name);
        }
    }
    if (!interesting) {
        InterlockedExchangePointer((PVOID volatile *)&neg_cache[slot], h);
        return 0;
    }
    dev_track(h, name);
    emit_handle(h, 2, name);
    return 1;
}

/* ------------------------------------------------------------------ pending IOCTLs */

#define MAX_PEND 256
#define PEND_TIMEOUT_MS 30000
#define STATUS_PENDING_ 0x00000103u

typedef struct pend {
    volatile LONG used;     /* 0 free, 1 being filled, 2 armed */
    uint32_t seq, code;
    uintptr_t iosb, out, handle;
    uint32_t outlen;
    ULONGLONG t0;
} pend;

static pend pend_tab[MAX_PEND];
static volatile LONG npend;
static SRWLOCK poll_lock = SRWLOCK_INIT;

int dev_pending_count(void)
{
    return npend;
}

static void emit_complete(const pend *p, uint32_t status, uint64_t info, uint32_t flags)
{
    nvt_buf b = {0};
    ntr_iocomplete r;
    memset(&r, 0, sizeof(r));
    r.call_seq = p->seq;
    r.status = status;
    r.information = info;
    r.handle = p->handle;
    r.out_addr = p->out;
    r.out_len = p->outlen;
    r.flags = flags;
    r.code = p->code;
    nvt_rec_begin(&b, NTR_REC_IOCOMPLETE);
    size_t hoff = b.len;
    nvt_buf_put(&b, &r, sizeof(r));
    uint64_t n = 0;
    if (!(flags & NTR_IOC_TIMEOUT) && p->out && info) {
        n = info < p->outlen ? info : p->outlen;
        if (n > nvt_cfg.maxbuf)
            n = nvt_cfg.maxbuf;
    }
    if (n && !nvt_buf_reserve(&b, (size_t)n)) {
        size_t got = nvt_safe_copy(b.p + b.len, (const void *)p->out, (size_t)n);
        b.len += got;
        ((ntr_iocomplete *)(b.p + hoff))->size = (uint32_t)got;
    }
    nvt_rec_end(&b);
    nvt_buf_free(&b);
}

static int read_iosb(uintptr_t iosb, uint32_t *status, uint64_t *info)
{
    uintptr_t v[2] = {0, 0};
    if (!nvt_safe_read(v, iosb, sizeof(v)))
        return 0;
    *status = (uint32_t)v[0];
    *info = v[1];
    return 1;
}

/* NtDeviceIoControlFile(FileHandle, Event, ApcRoutine, ApcContext,
 * IoStatusBlock, IoControlCode, InputBuffer, InputBufferLength,
 * OutputBuffer, OutputBufferLength) returned STATUS_PENDING */
void dev_pending_add(uintptr_t *args, uint32_t call_seq, int fsctl)
{
    (void)fsctl;
    dev_pending_add_buf(args, call_seq, args[8], (uint32_t)args[9], (uint32_t)args[5]);
}

/* any I/O call shaped (FileHandle, Event, Apc, ApcCtx, IoStatusBlock, ...)
 * that returned STATUS_PENDING; out/outlen = the buffer written later */
void dev_pending_add_buf(uintptr_t *args, uint32_t call_seq, uintptr_t out, uint32_t outlen,
                         uint32_t code)
{
    if (!nvt_cfg.iopending || !args[4])
        return;
    pend p;
    memset(&p, 0, sizeof(p));
    p.seq = call_seq;
    p.code = code;
    p.iosb = args[4];
    p.out = out;
    p.outlen = outlen;
    p.handle = args[0];
    p.t0 = GetTickCount64();
    uint32_t st;
    uint64_t info;
    if (read_iosb(p.iosb, &st, &info) && st != STATUS_PENDING_) {
        /* already completed (or the caller never initialised the IOSB) */
        emit_complete(&p, st, info, NTR_IOC_EARLY);
        return;
    }
    for (int i = 0; i < MAX_PEND; i++) {
        if (!InterlockedCompareExchange(&pend_tab[i].used, 1, 0)) {
            pend_tab[i].seq = p.seq;
            pend_tab[i].code = p.code;
            pend_tab[i].iosb = p.iosb;
            pend_tab[i].out = p.out;
            pend_tab[i].outlen = p.outlen;
            pend_tab[i].handle = p.handle;
            pend_tab[i].t0 = p.t0;
            InterlockedIncrement(&npend);
            InterlockedExchange(&pend_tab[i].used, 2);
            return;
        }
    }
    nvt_note(1, "pending IOCTL table full: completion of seq %u not recorded", call_seq);
}

void dev_pending_poll(int at_exit)
{
    if (!npend)
        return;
    if (!TryAcquireSRWLockExclusive(&poll_lock))
        return;
    ULONGLONG now = GetTickCount64();
    for (int i = 0; i < MAX_PEND; i++) {
        pend *p = &pend_tab[i];
        if (p->used != 2)
            continue;
        uint32_t st = STATUS_PENDING_;
        uint64_t info = 0;
        int readable = read_iosb(p->iosb, &st, &info);
        uint32_t flags = 0;
        if (readable && st != STATUS_PENDING_)
            flags = 0;
        else if (at_exit)
            flags = NTR_IOC_AT_EXIT | NTR_IOC_TIMEOUT;
        else if (!readable || now - p->t0 > PEND_TIMEOUT_MS)
            flags = NTR_IOC_TIMEOUT;
        else
            continue;
        emit_complete(p, st, info, flags);
        InterlockedDecrement(&npend);
        InterlockedExchange(&p->used, 0);
    }
    ReleaseSRWLockExclusive(&poll_lock);
}


/* ------------------------------------------------------------------ registry */

/* Key handles whose full name (NtQueryKey KeyNameInformation, so relative
 * opens and CurrentControlSet are resolved) matches NVTRACE_REG: by default
 * the display adapter class key, nvlddmkm's service key and anything NVIDIA
 * or DirectX/graphics-driver related. The UMDs read their configuration
 * there (registry keys of the adapter instance, D3D/OpenGL/Vulkan paths,
 * driver tweaks). Handles opened before injection are named on first use. */

static const char *const reg_default[] = {
    "{4d36e968-e325-11ce-bfc1-08002be10318}",   /* display adapter class */
    "\\services\\nvlddmkm", "\\services\\nvdisplay", "\\services\\nvsvc", "\\services\\nvvad",
    "\\services\\nvhda", "nvidia", "\\control\\graphicsdrivers", "\\control\\video\\",
    "\\microsoft\\direct3d", "\\microsoft\\directx", "\\microsoft\\directdraw", "\\opengldrivers",
    "\\enum\\pci\\ven_10de", "\\services\\dxgkrnl", "\\khronos\\vulkan", "\\dwm",
    NULL};

int reg_name_interesting(const char *n)
{
    if (nvt_cfg.reg >= 2)
        return 1;
    if (!nvt_cfg.reg)
        return 0;
    if (nvt_cfg.reg_match[0]) {
        const char *e = nvt_cfg.reg_match;
        while (*e) {
            const char *c = strchr(e, ',');
            size_t len = c ? (size_t)(c - e) : strlen(e);
            char part[128];
            if (len && len < sizeof(part)) {
                memcpy(part, e, len);
                part[len] = 0;
                if (strstr(n, part))
                    return 1;
            }
            if (!c)
                break;
            e = c + 1;
        }
        return 0;
    }
    for (int i = 0; reg_default[i]; i++)
        if (strstr(n, reg_default[i]))
            return 1;
    return 0;
}

typedef struct keyent {
    HANDLE h;
    char name[200];
} keyent;

#define MAX_KEYS 1024
static keyent *keytab;
static SRWLOCK key_lock = SRWLOCK_INIT;
static volatile LONG nkeys;
static HANDLE volatile key_neg[NEG_SLOTS];

typedef LONG(NTAPI *NtQueryKey_t)(HANDLE, ULONG, PVOID, ULONG, PULONG);
static NtQueryKey_t p_query_key;

int reg_tracked(HANDLE h)
{
    if (!nkeys || !h || !keytab)
        return 0;
    int found = 0;
    AcquireSRWLockShared(&key_lock);
    for (int i = 0; i < MAX_KEYS; i++)
        if (keytab[i].h == h) {
            found = 1;
            break;
        }
    ReleaseSRWLockShared(&key_lock);
    return found;
}

static int reg_name_of(HANDLE h, char *out, size_t outsz)
{
    union {
        ULONG len;
        uint8_t raw[2048];
    } u;
    ULONG ret = 0;
    out[0] = 0;
    if (!p_query_key || p_query_key(h, 3 /* KeyNameInformation */, &u, sizeof(u) - 2, &ret) < 0)
        return 0;
    const wchar_t *w = (const wchar_t *)(u.raw + 4);
    size_t n = u.len / 2, i;
    if (n > (sizeof(u) - 4) / 2)
        n = (sizeof(u) - 4) / 2;
    for (i = 0; i < n && i + 1 < outsz; i++) {
        wchar_t c = w[i];
        if (c >= 'A' && c <= 'Z')
            c = c - 'A' + 'a';
        out[i] = (c < 128 && c >= 32) ? (char)c : '?';
    }
    out[i] = 0;
    return 1;
}

static void key_track(HANDLE h, const char *name)
{
    if (!keytab)
        return;
    AcquireSRWLockExclusive(&key_lock);
    int slot = -1;
    for (int i = 0; i < MAX_KEYS; i++) {
        if (keytab[i].h == h) {
            slot = i;
            break;
        }
        if (slot < 0 && !keytab[i].h)
            slot = i;
    }
    if (slot >= 0) {
        if (!keytab[slot].h)
            InterlockedIncrement(&nkeys);
        keytab[slot].h = h;
        lstrcpynA(keytab[slot].name, name, sizeof(keytab[slot].name));
    }
    ReleaseSRWLockExclusive(&key_lock);
}

static void key_name_lookup(HANDLE h, char *out, size_t outsz)
{
    out[0] = 0;
    if (!keytab || !h)
        return;
    AcquireSRWLockShared(&key_lock);
    for (int i = 0; i < MAX_KEYS; i++)
        if (keytab[i].h == h) {
            lstrcpynA(out, keytab[i].name, (int)outsz);
            break;
        }
    ReleaseSRWLockShared(&key_lock);
}

int reg_probe(HANDLE h)
{
    if (reg_tracked(h))
        return 1;
    if (!nvt_cfg.reg || !h || h == INVALID_HANDLE_VALUE || (uintptr_t)h >= (uintptr_t)0x80000000u)
        return 0;   /* predefined HKEY_* values are not kernel handles */
    size_t slot = neg_slot(h);
    if (key_neg[slot] == h)
        return 0;
    char name[400];
    if (!reg_name_of(h, name, sizeof(name)) || !reg_name_interesting(name)) {
        InterlockedExchangePointer((PVOID volatile *)&key_neg[slot], h);
        return 0;
    }
    key_track(h, name);
    emit_handle(h, 3, name);
    return 1;
}

/* After NtOpenKey/NtOpenKeyEx/NtCreateKey. 1: keep the CALL record. */
int reg_after_open(HANDLE h, const char *oa_name, HANDLE root, int ok)
{
    char name[400];
    if (!nvt_cfg.reg)
        return 0;
    if (ok && h && reg_name_of(h, name, sizeof(name))) {
        if (!reg_name_interesting(name)) {
            InterlockedExchangePointer((PVOID volatile *)&key_neg[neg_slot(h)], h);
            return 0;
        }
        InterlockedCompareExchangePointer((PVOID volatile *)&key_neg[neg_slot(h)], NULL, h);
        key_track(h, name);
        emit_handle(h, 3, name);
        return 1;
    }
    /* failed open: the name as asked, relative to a known parent if any */
    char parent[200];
    key_name_lookup(root, parent, sizeof(parent));
    if (parent[0]) {
        _snprintf(name, sizeof(name) - 1, "%s\\%s", parent, oa_name);
        name[sizeof(name) - 1] = 0;
    } else {
        lstrcpynA(name, oa_name, sizeof(name));
    }
    return reg_name_interesting(name);
}

void reg_on_close(HANDLE h)
{
    InterlockedCompareExchangePointer((PVOID volatile *)&key_neg[neg_slot(h)], NULL, h);
    if (!nkeys || !keytab)
        return;
    AcquireSRWLockExclusive(&key_lock);
    for (int i = 0; i < MAX_KEYS; i++)
        if (keytab[i].h == h) {
            keytab[i].h = NULL;
            InterlockedDecrement(&nkeys);
        }
    ReleaseSRWLockExclusive(&key_lock);
}

void dev_init(void)
{
    p_query_object = (NtQueryObject_t)(void *)GetProcAddress(GetModuleHandleW(L"ntdll.dll"),
                                                             "NtQueryObject");
    p_query_key = (NtQueryKey_t)(void *)GetProcAddress(GetModuleHandleW(L"ntdll.dll"), "NtQueryKey");
    keytab = VirtualAlloc(NULL, MAX_KEYS * sizeof(keyent), MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE);
}
