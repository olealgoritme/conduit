/* Log writer. Records are copied straight into a writable mapped view of the
 * log file. A mapped file's dirty pages belong to the file, not to the
 * process, so everything written survives the process being killed
 * (TerminateProcess from outside, a crash, nvinject -t); no writer thread and
 * no in-process queue exist that could be lost.
 *
 * The file grows in segments (64 MiB on x64, 16 MiB on x86): each segment is
 * its own mapping object and view. A record never spans segments; the unused
 * tail of a segment is filled with a PAD record. Space is reserved under a
 * short lock, the copy happens outside it.
 *
 * Crash consistency: a record is written with type PAD first, then its body,
 * then the real type (one aligned 4-byte store). A reader therefore sees a
 * finished record, a PAD it skips, or zeros (end of data). On a clean exit
 * the file is truncated to its used length; after a kill it keeps a zero
 * tail that readers stop at.
 */
#include "nvtrace.h"
#include <stdio.h>
#include <stdarg.h>
#include <string.h>

#ifdef _WIN64
#define SEG_SIZE ((uint64_t)64 << 20)
#else
#define SEG_SIZE ((uint64_t)16 << 20)
#endif
#define GRAN 65536u

typedef struct segment {
    struct segment *next;   /* retired list */
    HANDLE map;
    uint8_t *view;
    uint64_t file_off;      /* file offset of view[0] */
    uint64_t len;
    uint64_t used;
    LONG refs;              /* writers copying into this view */
    int retired;
} segment;

static HANDLE log_file = INVALID_HANDLE_VALUE;
static SRWLOCK log_lock = SRWLOCK_INIT;
static segment *cur;
static segment *retired;        /* retired but still referenced */
static volatile LONG seq;
static volatile LONG log_closed;
static volatile LONG log_full;   /* size cap reached or disk full */
static volatile LONG exit_mode;  /* other threads were killed: try-lock only */
static uint64_t file_end;       /* end of the last segment */
static uint64_t max_bytes;

volatile LONG64 nvt_stat_records, nvt_stat_calls, nvt_stat_dropped, nvt_stat_bytes;

/* At process exit a thread may have been killed holding log_lock; then the
 * lock is only tried for a short while and the record dropped. */
volatile DWORD nvt_log_owner;   /* thread holding log_lock (hooks pass through) */

static int lock_log(void)
{
    if (!exit_mode) {
        AcquireSRWLockExclusive(&log_lock);
        nvt_log_owner = GetCurrentThreadId();
        return 1;
    }
    static volatile LONG lock_lost;
    if (lock_lost)
        return 0;
    for (int i = 0; i < 200; i++) {
        if (TryAcquireSRWLockExclusive(&log_lock)) {
            nvt_log_owner = GetCurrentThreadId();
            return 1;
        }
        Sleep(1);
    }
    InterlockedExchange(&lock_lost, 1);   /* held by a killed thread: give up */
    return 0;
}

static void unlock_log(void)
{
    nvt_log_owner = 0;
    ReleaseSRWLockExclusive(&log_lock);
}

void nvt_log_exit_mode(void)
{
    InterlockedExchange(&exit_mode, 1);
}

uint32_t nvt_next_seq(void)
{
    return (uint32_t)InterlockedIncrement(&seq);
}

static void seg_free(segment *s)
{
    if (s->view)
        UnmapViewOfFile(s->view);
    if (s->map)
        CloseHandle(s->map);
    VirtualFree(s, 0, MEM_RELEASE);
}

/* New segment covering [file_end, file_end + len). Called with the lock. */
static segment *seg_new(uint64_t need)
{
    uint64_t len = need > SEG_SIZE ? (need + GRAN - 1) / GRAN * GRAN : SEG_SIZE;
    uint64_t off = file_end;
    if (max_bytes && off + len > max_bytes)
        return NULL;
    segment *s = VirtualAlloc(NULL, sizeof(segment), MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE);
    if (!s)
        return NULL;
    memset(s, 0, sizeof(*s));
    uint64_t end = off + len;
    /* the mapping object extends the file to `end` (allocating its
     * clusters: a full disk fails here, never later in a page fault) */
    s->map = CreateFileMappingW(log_file, NULL, PAGE_READWRITE, (DWORD)(end >> 32), (DWORD)end, NULL);
    if (!s->map) {
        VirtualFree(s, 0, MEM_RELEASE);
        return NULL;
    }
    s->view = MapViewOfFile(s->map, FILE_MAP_WRITE, (DWORD)(off >> 32), (DWORD)off, (SIZE_T)len);
    if (!s->view) {
        CloseHandle(s->map);
        VirtualFree(s, 0, MEM_RELEASE);
        return NULL;
    }
    s->file_off = off;
    s->len = len;
    file_end = end;
    return s;
}

static void put_pad(uint8_t *p, uint64_t n)
{
    /* PAD: u32 type 0, u32 size; readers skip `size` bytes (>= 8) */
    uint32_t h[2] = {NTR_REC_PAD, (uint32_t)n};
    memcpy(p, h, 8);
}

/* Reserve `len` bytes; returns the destination and the segment to release. */
static uint8_t *reserve(size_t len, segment **segp)
{
    uint8_t *dst = NULL;
    if (!lock_log())
        return NULL;
    if (!cur || log_closed || log_full) {
        unlock_log();
        return NULL;
    }
    if (cur->len - cur->used < len) {
        segment *s = seg_new(len);
        if (!s) {
            /* stop for good only at the size cap or a full disk; an
             * oversized record whose view cannot be mapped (x86 address
             * space) is dropped alone */
            DWORD le = GetLastError();
            if ((max_bytes && file_end + len > max_bytes) || le == ERROR_DISK_FULL ||
                le == ERROR_HANDLE_DISK_FULL || len <= SEG_SIZE)
                InterlockedExchange(&log_full, 1);
            unlock_log();
            return NULL;
        }
        uint64_t rest = cur->len - cur->used;
        if (rest)
            put_pad(cur->view + cur->used, rest);
        cur->used = cur->len;
        cur->retired = 1;
        if (cur->refs == 0) {
            seg_free(cur);
        } else {
            cur->next = retired;
            retired = cur;
        }
        cur = s;
    }
    dst = cur->view + cur->used;
    /* PAD header under the lock: a writer killed before its copy leaves a
     * skippable PAD, never a zero hole that would end the readable data */
    put_pad(dst, len);
    cur->used += len;
    cur->refs++;
    *segp = cur;
    unlock_log();
    return dst;
}

static void release(segment *s)
{
    if (!lock_log())
        return;
    if (--s->refs == 0 && s->retired) {
        segment **pp = &retired;
        while (*pp && *pp != s)
            pp = &(*pp)->next;
        if (*pp)
            *pp = s->next;
        seg_free(s);
    }
    unlock_log();
}

int nvt_log_open(const wchar_t *path, uint32_t ptr_size, uint64_t maxlog)
{
    max_bytes = maxlog;
    log_file = CreateFileW(path, GENERIC_READ | GENERIC_WRITE, FILE_SHARE_READ, NULL, CREATE_NEW,
                           FILE_ATTRIBUTE_NORMAL, NULL);
    if (log_file == INVALID_HANDLE_VALUE)
        return -1;
    file_end = 0;
    cur = seg_new(SEG_SIZE);
    if (!cur) {
        DWORD le = GetLastError();
        CloseHandle(log_file);
        log_file = INVALID_HANDLE_VALUE;
        SetLastError(le ? le : ERROR_DISK_FULL);
        return -1;
    }
    ntr_file_header h;
    memset(&h, 0, sizeof(h));
    memcpy(h.magic, NTR_MAGIC, 8);
    h.version = NTR_VERSION;
    h.header_size = sizeof(h);
    h.ptr_size = ptr_size;
    h.pid = GetCurrentProcessId();
    h.qpc_freq = (uint64_t)nvt_qpc_freq.QuadPart;
    memcpy(cur->view, &h, sizeof(h));
    cur->used = sizeof(h);
    return 0;
}

int nvt_log_is_open(void)
{
    return log_file != INVALID_HANDLE_VALUE && !log_closed;
}

void nvt_log_submit(const void *rec, size_t len)
{
    segment *s;
    if (log_file == INVALID_HANDLE_VALUE || len < 8 || (len & 7)) {
        InterlockedIncrement64(&nvt_stat_dropped);
        return;
    }
    uint8_t *dst = reserve(len, &s);
    if (!dst) {
        InterlockedIncrement64(&nvt_stat_dropped);
        return;
    }
    memcpy(dst + 8, (const uint8_t *)rec + 8, len - 8);
    MemoryBarrier();
    InterlockedExchange((volatile LONG *)dst, (LONG) * (const uint32_t *)rec);
    release(s);
    InterlockedIncrement64(&nvt_stat_records);
    InterlockedAdd64(&nvt_stat_bytes, (LONG64)len);
}

/* Process exit, every other thread gone (DLL_PROCESS_DETACH at exit):
 * unmap and cut the file to its used length. With `truncate` 0 (other
 * threads may still be writing: TerminateProcess on ourselves) only the
 * views are flushed; the file keeps its zero tail. */
void nvt_log_close(int truncate)
{
    if (log_file == INVALID_HANDLE_VALUE || InterlockedExchange(&log_closed, 1))
        return;
    if (!truncate) {
        if (cur)
            FlushViewOfFile(cur->view, 0);
        return;
    }
    if (!lock_log()) {
        if (cur)
            FlushViewOfFile(cur->view, 0);
        return;
    }
    uint64_t end = cur ? cur->file_off + cur->used : 0;
    while (retired) {
        segment *s = retired;
        retired = s->next;
        seg_free(s);
    }
    if (cur) {
        seg_free(cur);
        cur = NULL;
    }
    unlock_log();
    LARGE_INTEGER li;
    li.QuadPart = (LONGLONG)end;
    if (end && SetFilePointerEx(log_file, li, NULL, FILE_BEGIN))
        SetEndOfFile(log_file);
    CloseHandle(log_file);
    log_file = INVALID_HANDLE_VALUE;
}

/* ------------------------------------------------------------------ buffers */

static HANDLE buf_heap;

/* ExitProcess kills the other threads; holding our heap's lock across that
 * guarantees no killed thread holds it (as Windows does for the process heap) */
void nvt_heap_lock(int lock)
{
    if (!buf_heap)
        return;
    if (lock)
        HeapLock(buf_heap);
    else
        HeapUnlock(buf_heap);
}

int nvt_buf_reserve(nvt_buf *b, size_t extra)
{
    if (b->oom)
        return -1;
    if (b->len + extra <= b->cap)
        return 0;
    if (b->fixed) {
        b->oom = 1;
        return -1;
    }
    if (extra > ((size_t)1 << (sizeof(size_t) == 8 ? 40 : 30)) - b->len) {
        b->oom = 1;
        return -1;
    }
    size_t ncap = b->cap ? b->cap : 4096;
    while (ncap < b->len + extra)
        ncap *= 2;
    if (!buf_heap) {
        HANDLE hp = HeapCreate(0, 0, 0);
        if (InterlockedCompareExchangePointer(&buf_heap, hp, NULL))
            HeapDestroy(hp);
    }
    uint8_t *np = b->p ? HeapReAlloc(buf_heap, 0, b->p, ncap) : HeapAlloc(buf_heap, 0, ncap);
    if (!np) {
        b->oom = 1;
        return -1;
    }
    b->p = np;
    b->cap = ncap;
    return 0;
}

void nvt_buf_free(nvt_buf *b)
{
    if (b->fixed) {
        b->len = 0;
        b->oom = 0;
        return;
    }
    if (b->p && buf_heap)
        HeapFree(buf_heap, 0, b->p);
    b->p = NULL;
    b->len = b->cap = 0;
    b->oom = 0;
}

void *nvt_buf_put(nvt_buf *b, const void *p, size_t n)
{
    if (nvt_buf_reserve(b, n))
        return NULL;
    void *d = b->p + b->len;
    if (p)
        memcpy(d, p, n);
    else
        memset(d, 0, n);
    b->len += n;
    return d;
}

void nvt_buf_pad8(nvt_buf *b)
{
    static const uint8_t z[8];
    size_t pad = (8 - (b->len & 7)) & 7;
    if (pad)
        nvt_buf_put(b, z, pad);
}

void nvt_rec_begin(nvt_buf *b, uint32_t type)
{
    ntr_rec_hdr h;
    LARGE_INTEGER q;
    b->len = 0;
    b->oom = 0;
    QueryPerformanceCounter(&q);
    h.type = type;
    h.size = 0;
    h.qpc = (uint64_t)q.QuadPart;
    h.tid = GetCurrentThreadId();
    h.seq = 0;
    nvt_buf_put(b, &h, sizeof(h));
}

uint32_t nvt_rec_end(nvt_buf *b)
{
    nvt_buf_pad8(b);
    if (b->oom || b->len < sizeof(ntr_rec_hdr) || b->len > 0xFFFFFFF8u) {
        InterlockedIncrement64(&nvt_stat_dropped);
        return 0;
    }
    ntr_rec_hdr *h = (ntr_rec_hdr *)b->p;
    h->size = (uint32_t)b->len;
    if (!h->seq)
        h->seq = nvt_next_seq();
    nvt_log_submit(b->p, b->len);
    return h->seq;
}

void nvt_note(int level, const char *fmt, ...)
{
    char text[1024];
    va_list ap;
    va_start(ap, fmt);
    int n = _vsnprintf(text, sizeof(text) - 1, fmt, ap);
    va_end(ap);
    if (n < 0 || n > (int)sizeof(text) - 1)
        n = sizeof(text) - 1;
    text[n] = 0;
    nvt_buf b = {0};
    uint32_t hdr[2] = {(uint32_t)level, 0};
    nvt_rec_begin(&b, NTR_REC_NOTE);
    nvt_buf_put(&b, hdr, sizeof(hdr));
    nvt_buf_put(&b, text, (size_t)n + 1);
    nvt_rec_end(&b);
    nvt_buf_free(&b);
}
