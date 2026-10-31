/* nvtrace internal declarations. */
#ifndef NVTRACE_H
#define NVTRACE_H

#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <stdint.h>
#include <stddef.h>

#include "ntr_format.h"

#define NVTRACE_VERSION "1.0"

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

enum { NVT_SZ_FIXED = 0, NVT_SZ_FIELD = 1, NVT_SZ_WSTR = 2 };
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
};

/* ---- hooks */
#define NVT_MAX_ARGS 16

typedef struct nvt_hook nvt_hook;
typedef uintptr_t (*nvt_handler)(nvt_hook *h, uintptr_t *args, void *ret);

struct nvt_hook {
    void *orig;             /* trampoline: call this to run the original */
    void *target;           /* hooked address */
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
int nvt_hook_install(nvt_hook *h, uint32_t *err);
uint32_t nvt_stub_syscall_nr(const uint8_t *p);
int nvt_stub_argbytes(const uint8_t *p, uint32_t *argbytes);
int nvt_insn_len(const uint8_t *p, int is64);   /* for tests */
void nvt_hooks_begin(void);
void nvt_hooks_end(void);

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
} nvt_buf;

int nvt_log_open(const wchar_t *path, uint32_t ptr_size);
void nvt_log_submit(const void *rec, size_t len);
void nvt_log_flush_final(void);
void nvt_log_sync(DWORD timeout_ms);
uint32_t nvt_next_seq(void);
int nvt_buf_reserve(nvt_buf *b, size_t extra);
void nvt_buf_free(nvt_buf *b);
void *nvt_buf_put(nvt_buf *b, const void *p, size_t n);
void nvt_buf_pad8(nvt_buf *b);
void nvt_rec_begin(nvt_buf *b, uint32_t type);
void nvt_rec_end(nvt_buf *b);   /* fixes size, submits */
void nvt_note(int level, const char *fmt, ...);
extern volatile LONG64 nvt_stat_records, nvt_stat_calls, nvt_stat_dropped, nvt_stat_bytes;

/* config */
typedef struct nvt_config {
    wchar_t dir[MAX_PATH];
    uint32_t rawsize;      /* NVTRACE_RAWSIZE: raw arg0 bytes for unknown functions */
    uint64_t maxbuf;       /* NVTRACE_MAXBUF: cap per copied buffer */
    uint32_t stack;        /* NVTRACE_STACK: return addresses per call */
    int devices_all;       /* NVTRACE_DEVICES=all */
    int files;             /* NVTRACE_FILES=1: log every NtCreateFile/NtOpenFile */
    int maps;              /* NVTRACE_MAPS=0 disables section map logging */
    int follow;            /* NVTRACE_FOLLOW=1 */
    uint32_t chase;        /* NVTRACE_CHASE: bytes per chased pointer, 0 = off */
    uint32_t chase_depth;  /* NVTRACE_CHASE_DEPTH */
    uint32_t chase_max;    /* NVTRACE_CHASE_MAX: pointers chased per buffer */
    uint32_t cmdbuf;       /* NVTRACE_CMDBUF: max bytes of Render command buffer */
    char exclude[1024];    /* NVTRACE_EXCLUDE: comma separated function names */
    int ntdll;             /* NVTRACE_NTDLL=0 disables ntdll hooks */
} nvt_config;

extern nvt_config nvt_cfg;
extern volatile LONG nvt_dead;
extern LARGE_INTEGER nvt_qpc_freq;
extern DWORD nvt_tls;

#endif
