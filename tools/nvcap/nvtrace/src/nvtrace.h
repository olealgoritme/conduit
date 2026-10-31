/* nvtrace internal declarations. */
#ifndef NVTRACE_H
#define NVTRACE_H

#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <stdint.h>
#include <stddef.h>

#include "ntr_format.h"
#include "x86dec.h"

#define NVTRACE_VERSION "2.0"

/* ---- generated layout tables (layouts_gen.h) */
typedef struct nvt_pdesc {
    uint16_t off;      /* offset of the pointer field in the parent element */
    uint16_t szoff;    /* offset of the size/count field in the parent element */
    uint8_t kind;      /* NVT_SZ_* */
    uint8_t szw;       /* width of the size field in bytes */
    uint8_t flags;     /* NVT_PF_* */
    uint8_t nchild;    /* number of child descriptors */
    uint32_t fixed;    /* FIXED: byte count; FIELD: multiplier */
    uint32_t elem;     /* element stride for children (0 = one element) */
    uint16_t label;    /* index into nvt_labels */
    uint16_t child;    /* index of first child in nvt_pdescs */
} nvt_pdesc;

enum { NVT_SZ_FIXED = 0, NVT_SZ_FIELD = 1, NVT_SZ_WSTR = 2, NVT_SZ_FIELD1 = 3 };
enum { NVT_PF_IN = 1, NVT_PF_OUT = 2, NVT_PF_CHASE = 4 };

typedef struct nvt_fdesc {
    const char *module;
    const char *name;
    uint16_t nargs;
    uint16_t pfirst;
    uint16_t pcount;
    uint16_t flags;    /* NVT_FN_* */
    uint32_t arg0size;
} nvt_fdesc;

enum {
    NVT_FN_OPEN = 1,       /* NtCreateFile/NtOpenFile: track device handles */
    NVT_FN_DEVHANDLE = 2,  /* log only for tracked device handles (arg0) */
    NVT_FN_CLOSE = 4,      /* NtClose: log only for tracked handles */
    NVT_FN_MAP = 8,        /* section mapping (config.maps) */
    NVT_FN_CTXTRACK = 16,  /* D3DKMT CreateContext: remember command buffer */
    NVT_FN_RENDER = 32,    /* D3DKMT Render: copy the command buffer */
    NVT_FN_PROCESS = 64,   /* CreateProcessInternalW (follow children) */
    NVT_FN_RAW = 128,      /* unknown layout: raw dump of arg0 */
    NVT_FN_TERMINATE = 256, /* NtTerminateProcess: flush the log first */
    NVT_FN_REGOPEN = 512,   /* NtOpenKey/NtCreateKey: log and track interesting keys */
    NVT_FN_REGKEY = 1024,   /* registry call on a key handle: log for tracked keys */
};

/* what gpumem.c / devio.c do around a call (set from the function name) */
enum {
    GK_NONE = 0,
    GK_LOCK, GK_LOCK2, GK_UNLOCK, GK_UNLOCK2,
    GK_CREATEALLOC, GK_CREATEALLOC2, GK_DESTROYALLOC,
    GK_MAPGPUVA, GK_UPDATEGPUVA, GK_FREEGPUVA,
    GK_SUBMIT, GK_SUBMITHWQ,
    GK_CREATEDOORBELL, GK_DESTROYDOORBELL, GK_NOTIFYWORK,
    GK_SYNCPOINT,          /* fence signal/wait from GPU, presents: sampler sync point */
    GK_CREATEHWQUEUE, GK_CREATEPAGINGQUEUE, GK_CREATESYNC2, GK_CREATENATIVEFENCE,
    GK_OPENNATIVEFENCE, GK_OPENSYNC2,
    GK_OPENRESOURCE, GK_OPENRESOURCENT,
    GK_ESCAPE,
    GK_IOCTL,              /* NtDeviceIoControlFile / NtFsControlFile */
    GK_IOREAD, GK_IOWRITE, /* NtReadFile / NtWriteFile on device handles */
};

/* ---- hooks */
#define NVT_MAX_ARGS 16

typedef struct nvt_hook nvt_hook;
typedef uintptr_t (*nvt_handler)(nvt_hook *h, uintptr_t *args, void *ret);

struct nvt_hook {
    void *orig;             /* trampoline: call this to run the original */
    void *target;           /* hooked address */
    uint8_t *entry;         /* entry stub (the patch jumps here) */
    uint8_t *tramp;         /* relocated prologue */
    uint32_t consumed;      /* prologue bytes relocated */
    uint32_t installed;
    uint32_t gk;            /* GK_* */
    uint32_t calls;         /* selftest counter */
    x86_relocmap relmap;    /* instruction boundaries prologue -> trampoline */
    uint8_t orig_bytes[24]; /* prologue at prepare time */
    const nvt_fdesc *fd;    /* layout, NULL for raw */
    nvt_handler handler;
    uint32_t id;
    uint32_t nargs;         /* argument slots captured and passed on */
    uint32_t argbytes;      /* x86: bytes the callee pops (stdcall), x64 unused */
    uint32_t syscall_nr;
    uint32_t flags;         /* NVT_FN_* */
    uint32_t hookflags;     /* NTR_HOOK_* */
    char name[80];
    char module[32];
};

/* hook.c */
int nvt_hook_prepare(nvt_hook *h, uint32_t *err);
/* patches all prepared hooks with the other threads suspended; per-hook
 * error in errs[i]; *moved = threads moved out of patched bytes */
int nvt_hooks_commit(nvt_hook **hs, int n, uint32_t *errs, uint32_t *moved);
void *nvt_process_list(void);   /* SystemProcessInformation, VirtualFree it */
uint32_t nvt_stub_syscall_nr(const uint8_t *p);
int nvt_stub_argbytes(const uint8_t *p, uint32_t *argbytes);
int nvt_insn_len(const uint8_t *p, int is64);   /* for tests */

/* asm */
uintptr_t nvt_call_orig(void *fn, const uintptr_t *args, uint32_t nargs);
size_t nvt_safe_copy(void *dst, const void *src, size_t n);   /* nvtrace.c */
extern char nvt_entry_common[];

/* log.c */
typedef struct nvt_buf {
    uint8_t *p;
    size_t len;
    size_t cap;
    int oom;
    int fixed;      /* p is caller storage (stack): never reallocated or freed */
} nvt_buf;

int nvt_log_open(const wchar_t *path, uint32_t ptr_size, uint64_t maxlog);
int nvt_log_is_open(void);
void nvt_log_submit(const void *rec, size_t len);
void nvt_log_close(int truncate);
void nvt_log_exit_mode(void);
extern volatile DWORD nvt_log_owner;
void nvt_heap_lock(int lock);
extern volatile LONG nvt_exiting;   /* ExitProcess killed the other threads */   /* process exit: never block on a lock a dead thread held */
uint32_t nvt_next_seq(void);
int nvt_buf_reserve(nvt_buf *b, size_t extra);
void nvt_buf_free(nvt_buf *b);
void *nvt_buf_put(nvt_buf *b, const void *p, size_t n);
void nvt_buf_pad8(nvt_buf *b);
void nvt_rec_begin(nvt_buf *b, uint32_t type);
uint32_t nvt_rec_end(nvt_buf *b);   /* fixes size, submits; returns the record's seq */
void nvt_note(int level, const char *fmt, ...);
extern volatile LONG64 nvt_stat_records, nvt_stat_calls, nvt_stat_dropped, nvt_stat_bytes;

/* config */
typedef struct nvt_config {
    wchar_t dir[MAX_PATH];
    uint32_t rawsize;      /* NVTRACE_RAWSIZE: raw arg0 bytes for unknown functions */
    uint64_t maxbuf;       /* NVTRACE_MAXBUF: cap per copied buffer */
    uint64_t maxrec;       /* NVTRACE_MAXREC: cap per CALL record (all its blobs) */
    uint32_t stack;        /* NVTRACE_STACK: return addresses per call */
    int devices_all;       /* NVTRACE_DEVICES=all */
    int files;             /* NVTRACE_FILES: 0 devices only, 1 NVIDIA-related files too (default), 2 every open */
    int maps;              /* NVTRACE_MAPS=0 disables section map logging */
    int follow;            /* NVTRACE_FOLLOW=1 */
    uint32_t chase;        /* NVTRACE_CHASE: bytes per chased pointer, 0 = off */
    uint32_t chase_depth;  /* NVTRACE_CHASE_DEPTH */
    uint32_t chase_max;    /* NVTRACE_CHASE_MAX: pointers chased per buffer */
    uint32_t cmdbuf;       /* NVTRACE_CMDBUF: max bytes of Render command buffer */
    char exclude[1024];    /* NVTRACE_EXCLUDE: comma separated function names */
    int ntdll;             /* NVTRACE_NTDLL=0 disables ntdll hooks */
    int lazy;              /* NVTRACE_MODE=lazy: start when a trigger module loads */
    char triggers[512];    /* NVTRACE_TRIGGER: module name prefixes (lower case) */
    uint64_t maxlog;       /* NVTRACE_MAXLOG: log size cap in bytes, 0 = none */
    uint32_t pushbuf;      /* NVTRACE_PUSHBUF: bytes copied per submission, 0 = off */
    int gpfifo;            /* NVTRACE_GPFIFO: follow GPFIFO-like command buffers */
    int snap;              /* NVTRACE_SNAP: 0 off, 1 learned regions, 2 + every mapped allocation */
    uint32_t snap_ms;      /* NVTRACE_SNAP_MS: sampler period */
    uint32_t snap_us;      /* NVTRACE_SNAP_US: sampler time budget per period */
    uint64_t snap_region;  /* NVTRACE_SNAP_REGION: max bytes watched per region */
    char watch[512];       /* NVTRACE_WATCH: gpuva[:size],... to sample */
    int iopending;         /* NVTRACE_IOPENDING: report late IOCTL completions */
    int probe;             /* NVTRACE_PROBE: name untracked handles on first IOCTL */
    int regions;           /* NVTRACE_REGIONS: REGION records for returned pointers */
    int selftest;          /* NVTRACE_SELFTEST: started by nvinject --selftest */
    int reg;               /* NVTRACE_REG: 0 off, 1 matching keys, 2 every key */
    char reg_match[1024];  /* NVTRACE_REG (list): lower-case substrings of key names */
    int snap_submit;       /* NVTRACE_SNAP_SUBMIT: pass over the command allocation at each submit */
    uint32_t snap_submit_us; /* NVTRACE_SNAP_SUBMIT_US: time budget of that pass */
} nvt_config;

/* nvtrace.c */
typedef struct capture capture;
extern volatile LONG nvt_log_ready;
int nvt_safe_read(void *dst, uintptr_t src, size_t n);
int nvt_user_ptr_like(uint64_t v);
void nvt_mark_thread_internal(void);
int nvt_thread_enter(void);        /* 1: this thread may log (sets busy) */
void nvt_thread_leave(void);

/* gpumem.c: GPU VA map, command buffer capture, page sampler, regions */
void gm_init(void);
void gm_before(nvt_hook *h, uintptr_t *args, nvt_buf *gb);   /* phase 0 */
void gm_after(nvt_hook *h, uintptr_t *args, int ok, uint32_t call_seq);
void gm_flush_pending(nvt_buf *gb, uint32_t call_seq);        /* emits stashed GPUMEM */
void gm_sample_tick(int sync);
int gm_has_watch(void);
void gm_region(uintptr_t addr, uint32_t source, uint32_t call_seq, uint32_t handle, uint32_t offset);
void gm_scan_returned(const uint8_t *before, const uint8_t *after, size_t len, uint32_t source,
                      uint32_t call_seq);

/* devio.c: device handles, pending IOCTLs */
void dev_init(void);
int dev_tracked(HANDLE h);
void dev_track(HANDLE h, const char *name);
void dev_untrack(HANDLE h);
void dev_on_close(HANDLE h);
int dev_probe(HANDLE h);           /* 1 if h is (now) a tracked device handle */
int dev_name_interesting(const char *lower_name);
void dev_pending_add(uintptr_t *args, uint32_t call_seq, int fsctl);
void dev_pending_poll(int at_exit);
int dev_pending_count(void);
void dev_pending_add_buf(uintptr_t *args, uint32_t call_seq, uintptr_t out, uint32_t outlen, uint32_t code);

/* registry keys (devio.c) */
int reg_tracked(HANDLE h);
int reg_probe(HANDLE h);                    /* 1 if h is (now) a tracked key */
int reg_after_open(HANDLE h, const char *oa_name, HANDLE root, int ok);   /* 1: keep the record */
void reg_on_close(HANDLE h);
int reg_name_interesting(const char *lower_name);

/* NVIDIA module address ranges (nvtrace.c), for "called from an NVIDIA DLL" */
int nv_module_at(uintptr_t addr);
void gm_snap_full(uintptr_t cpu);           /* every page of the watch holding cpu */
void gm_snap_rings(void);                   /* every page of doorbell ring watches */

extern nvt_config nvt_cfg;
extern volatile LONG nvt_dead;
extern LARGE_INTEGER nvt_qpc_freq;
extern DWORD nvt_tls;

#endif
