/* Log writer: producers append finished records to in-memory chunks under a
 * critical section; one writer thread writes them to the file. */
#include "nvtrace.h"
#include <stdio.h>
#include <stdarg.h>
#include <string.h>

typedef struct chunk {
    struct chunk *next;
    size_t len;
    size_t cap;
    uint8_t data[];
} chunk;

#define CHUNK_SIZE (4u << 20)
#define QUEUE_LIMIT ((LONG64)512 << 20)

static HANDLE log_file = INVALID_HANDLE_VALUE;
static HANDLE log_event;
static HANDLE log_thread;
static CRITICAL_SECTION log_lock;
static chunk *cur;
static chunk *full_head, *full_tail;
static volatile LONG64 queued;
static volatile LONG stop_writer;
static volatile LONG seq;
static volatile LONG final_mode;
/* chunk list the writer is working on, and bytes of its head already written:
 * if the writer is killed at process exit, the final flush resumes there so
 * no record is torn (at worst one chunk is written twice; readers drop
 * records with a repeated sequence number). */
static chunk *volatile inflight;
static volatile size_t inflight_done;
static DWORD writer_tid;

volatile LONG64 nvt_stat_records, nvt_stat_calls, nvt_stat_dropped, nvt_stat_bytes;

uint32_t nvt_next_seq(void)
{
    return (uint32_t)InterlockedIncrement(&seq);
}

static chunk *chunk_alloc(size_t need)
{
    size_t cap = need > CHUNK_SIZE ? need : CHUNK_SIZE;
    chunk *c = VirtualAlloc(NULL, sizeof(chunk) + cap, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE);
    if (!c)
        return NULL;
    c->next = NULL;
    c->len = 0;
    c->cap = cap;
    return c;
}

static void write_all(const void *p, size_t n, volatile size_t *progress)
{
    const uint8_t *b = p;
    while (n) {
        DWORD w = 0;
        DWORD part = n > (1u << 30) ? (1u << 30) : (DWORD)n;
        if (!WriteFile(log_file, b, part, &w, NULL) || w == 0)
            return;
        if (progress)
            *progress += w;
        b += w;
        n -= w;
        InterlockedAdd64(&nvt_stat_bytes, w);
    }
}

/* write and free the inflight list */
static void drain_inflight(void)
{
    while (inflight) {
        chunk *c = inflight;
        size_t done = inflight_done;
        if (done < c->len)
            write_all(c->data + done, c->len - done, &inflight_done);
        InterlockedAdd64(&queued, -(LONG64)c->len);
        /* reset progress first: a kill between the two lines then repeats
         * this whole chunk (dropped by readers) instead of tearing the next */
        inflight_done = 0;
        inflight = c->next;
        VirtualFree(c, 0, MEM_RELEASE);
    }
}

static DWORD WINAPI writer_main(LPVOID arg)
{
    (void)arg;
    TlsSetValue(nvt_tls, (void *)1);   /* marks this thread: hooks pass through */
    for (;;) {
        WaitForSingleObject(log_event, 50);
        EnterCriticalSection(&log_lock);
        if (cur && cur->len) {
            if (full_tail)
                full_tail->next = cur;
            else
                full_head = cur;
            full_tail = cur;
            cur = NULL;
        }
        inflight_done = 0;
        inflight = full_head;
        full_head = full_tail = NULL;
        LeaveCriticalSection(&log_lock);
        drain_inflight();
        if (stop_writer)
            break;
    }
    return 0;
}

static HANDLE buf_heap;

int nvt_log_open(const wchar_t *path, uint32_t ptr_size)
{
    InitializeCriticalSection(&log_lock);
    if (!buf_heap)
        buf_heap = HeapCreate(0, 0, 0);
    log_file = CreateFileW(path, GENERIC_WRITE, FILE_SHARE_READ, NULL, CREATE_NEW,
                           FILE_ATTRIBUTE_NORMAL, NULL);
    if (log_file == INVALID_HANDLE_VALUE)
        return -1;
    ntr_file_header h;
    memset(&h, 0, sizeof(h));
    memcpy(h.magic, NTR_MAGIC, 8);
    h.version = NTR_VERSION;
    h.header_size = sizeof(h);
    h.ptr_size = ptr_size;
    h.pid = GetCurrentProcessId();
    h.qpc_freq = (uint64_t)nvt_qpc_freq.QuadPart;
    write_all(&h, sizeof(h), NULL);
    log_event = CreateEventW(NULL, FALSE, FALSE, NULL);
    log_thread = CreateThread(NULL, 0, writer_main, NULL, 0, &writer_tid);
    return 0;
}

void nvt_log_submit(const void *rec, size_t len)
{
    if (log_file == INVALID_HANDLE_VALUE)
        return;
    InterlockedIncrement64(&nvt_stat_records);
    if (final_mode) {
        write_all(rec, len, NULL);
        return;
    }
    /* back-pressure: let the writer catch up rather than eat all memory */
    if (queued > QUEUE_LIMIT && GetCurrentThreadId() != writer_tid && !stop_writer) {
        ULONGLONG deadline = GetTickCount64() + 2000;
        while (queued > QUEUE_LIMIT && GetTickCount64() < deadline) {
            SetEvent(log_event);
            Sleep(1);
        }
    }
    EnterCriticalSection(&log_lock);
    if (!cur || cur->cap - cur->len < len) {
        if (cur) {
            if (full_tail)
                full_tail->next = cur;
            else
                full_head = cur;
            full_tail = cur;
        }
        cur = chunk_alloc(len);
        if (!cur) {
            LeaveCriticalSection(&log_lock);
            InterlockedIncrement64(&nvt_stat_dropped);
            return;
        }
    }
    memcpy(cur->data + cur->len, rec, len);
    cur->len += len;
    LeaveCriticalSection(&log_lock);
    if (InterlockedAdd64(&queued, (LONG64)len) > (LONG64)CHUNK_SIZE)
        SetEvent(log_event);
}

/* Process exit. Either the other threads are gone (ExitProcess; the writer
 * may have died holding the lock) or they still run (TerminateProcess on
 * ourselves). Stop the writer first so only this thread drains; afterwards
 * every record is written directly (final mode). */
void nvt_log_flush_final(void)
{
    if (log_file == INVALID_HANDLE_VALUE || final_mode)
        return;
    InterlockedExchange(&stop_writer, 1);
    if (log_thread) {
        SetEvent(log_event);
        if (WaitForSingleObject(log_thread, 2000) != WAIT_OBJECT_0)
            return;                     /* writer stuck in a write: do not race it */
    }
    int locked = TryEnterCriticalSection(&log_lock);
    for (ULONGLONG deadline = GetTickCount64() + 200; !locked && GetTickCount64() < deadline;) {
        Sleep(1);
        locked = TryEnterCriticalSection(&log_lock);
    }
    InterlockedExchange(&final_mode, 1);
    drain_inflight();
    chunk *list = full_head;
    full_head = full_tail = NULL;
    if (cur) {
        cur->next = NULL;
        if (list) {
            chunk *t = list;
            while (t->next)
                t = t->next;
            t->next = cur;
        } else {
            list = cur;
        }
        cur = NULL;
    }
    if (locked)
        LeaveCriticalSection(&log_lock);
    inflight_done = 0;
    inflight = list;
    drain_inflight();
}

/* Wait until the writer has written everything queued so far. */
void nvt_log_sync(DWORD timeout_ms)
{
    if (log_file == INVALID_HANDLE_VALUE || final_mode || GetCurrentThreadId() == writer_tid)
        return;
    ULONGLONG deadline = GetTickCount64() + timeout_ms;
    while ((queued > 0 || inflight) && GetTickCount64() < deadline) {
        SetEvent(log_event);
        Sleep(1);
    }
}

/* ------------------------------------------------------------------ buffers */

int nvt_buf_reserve(nvt_buf *b, size_t extra)
{
    if (b->oom)
        return -1;
    if (b->len + extra <= b->cap)
        return 0;
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

void nvt_rec_end(nvt_buf *b)
{
    nvt_buf_pad8(b);
    if (b->oom || b->len < sizeof(ntr_rec_hdr)) {
        InterlockedIncrement64(&nvt_stat_dropped);
        return;
    }
    ntr_rec_hdr *h = (ntr_rec_hdr *)b->p;
    h->size = (uint32_t)b->len;
    h->seq = nvt_next_seq();
    nvt_log_submit(b->p, b->len);
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
