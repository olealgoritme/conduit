/* nvtrace log (.ntr) record format, version 1. Little-endian.
 *
 * File = ntr_file_header, then records back to back. Every record starts with
 * ntr_rec_hdr and its total size is a multiple of 8. Readers skip record
 * types they do not know. Strings are UTF-8, NUL-terminated.
 */
#ifndef NTR_FORMAT_H
#define NTR_FORMAT_H

#include <stdint.h>

#define NTR_MAGIC "NVTRACE\0"
#define NTR_VERSION 1

typedef struct ntr_file_header {
    char magic[8];         /* "NVTRACE\0" */
    uint32_t version;      /* NTR_VERSION */
    uint32_t header_size;  /* sizeof(ntr_file_header) = 32 */
    uint32_t ptr_size;     /* 4 or 8: pointer size of the traced process */
    uint32_t pid;
    uint64_t qpc_freq;     /* QueryPerformanceFrequency */
} ntr_file_header;

typedef struct ntr_rec_hdr {
    uint32_t type;         /* NTR_REC_* */
    uint32_t size;         /* whole record incl. this header, multiple of 8 */
    uint64_t qpc;          /* QueryPerformanceCounter at record start */
    uint32_t tid;          /* thread id */
    uint32_t seq;          /* per-process record sequence number */
} ntr_rec_hdr;             /* 24 bytes */

enum {
    /* body: "key=value\0" strings, terminated by an empty string. Keys:
     * os (major.minor.build), os_ubr, ptr_size, pid, ppid, exe, cmdline, cwd,
     * nvtrace (version), dll (path of the nvtrace dll), qpc_freq, start_filetime,
     * config.* (effective settings) */
    NTR_REC_PROCINFO = 1,
    /* body: u32 id, then the string. Ids are label ids of blobs. */
    NTR_REC_STRING = 2,
    /* body: ntr_hook, then module\0 function\0 */
    NTR_REC_HOOK = 3,
    /* body: ntr_module, then path\0 */
    NTR_REC_MODULE = 4,
    /* body: u64 base, then path\0 */
    NTR_REC_MODULE_UNLOAD = 5,
    /* body: ntr_call, args, frames, then ntr_blob entries up to the end */
    NTR_REC_CALL = 6,
    /* body: u32 level (0 info, 1 warning, 2 error), u32 0, then text\0 */
    NTR_REC_NOTE = 7,
    /* body: ntr_end */
    NTR_REC_END = 8,
};

/* NTR_REC_HOOK */
typedef struct ntr_hook {
    uint32_t hook_id;      /* referenced by ntr_call.hook_id */
    uint32_t flags;        /* NTR_HOOK_* */
    uint32_t syscall_nr;   /* from the stub's mov eax, imm32; 0xFFFFFFFF if none */
    uint32_t nargs;        /* argument slots stored per call */
    uint64_t address;      /* address of the hooked function */
    uint32_t arg0_size;    /* size of the known struct arg0 points to (0 = none/raw) */
    uint32_t status;       /* 0 ok, else NTR_HOOKERR_* */
} ntr_hook;                /* 32 bytes */

enum {
    NTR_HOOK_OK = 1,          /* the hook is installed */
    NTR_HOOK_LAYOUT = 2,      /* a known layout drives the deep copies */
    NTR_HOOK_RAW = 4,         /* unknown layout: arg0 raw dump only */
    NTR_HOOK_SYSCALL_STUB = 8,
};

enum {
    NTR_HOOKERR_NOTFOUND = 1,
    NTR_HOOKERR_DECODE = 2,   /* prologue not relocatable */
    NTR_HOOKERR_ALLOC = 3,
    NTR_HOOKERR_PROTECT = 4,
    NTR_HOOKERR_EXCLUDED = 5,
};

/* NTR_REC_MODULE */
typedef struct ntr_module {
    uint64_t base;
    uint64_t size;
    uint16_t version[4];   /* file version major, minor, build, private (0 if none) */
    uint32_t timestamp;    /* PE header TimeDateStamp */
    uint32_t flags;        /* NTR_MOD_* */
} ntr_module;              /* 32 bytes */

enum {
    NTR_MOD_INITIAL = 1,   /* already loaded when nvtrace started */
    NTR_MOD_LOADED = 2,    /* loader notification */
    NTR_MOD_FINAL = 4,     /* snapshot at process exit */
};

/* NTR_REC_CALL. rec_hdr.qpc is the time just before the original function
 * was called. */
typedef struct ntr_call {
    uint32_t hook_id;
    uint32_t flags;        /* NTR_CALL_* */
    uint64_t qpc_end;      /* right after the original returned */
    uint64_t retval;       /* return value (NTSTATUS sign-extended) */
    uint16_t nargs;        /* u64 args[nargs] follow (zero-extended on x86) */
    uint16_t nframes;      /* u64 frames[nframes] follow: return addresses,
                              frames[0] = immediate caller */
    uint32_t depth;        /* nesting depth of traced calls on this thread */
} ntr_call;                /* 32 bytes */

enum {
    NTR_CALL_RAW = 1,          /* arg0 copied raw (no known layout) */
    NTR_CALL_TRUNCATED = 2,    /* some blob hit a size cap */
    NTR_CALL_FAULT = 4,        /* some blob read faulted (partial copy) */
};

/* Blob entry inside a CALL record: a copy of memory the call referenced. */
typedef struct ntr_blob {
    uint32_t label;        /* string id (NTR_REC_STRING) naming the field path */
    uint32_t index;        /* element index in the parent array, 0xFFFFFFFF if none */
    uint64_t addr;         /* source address in the traced process */
    uint32_t req_size;     /* bytes the layout asked for (before capping) */
    uint32_t cap_size;     /* bytes stored after this header (then pad to 8) */
    uint8_t phase;         /* 0 = before the call, 1 = after */
    uint8_t flags;         /* NTR_BLOB_* */
    uint16_t parent;       /* 1-based index of the parent blob in this record, 0 = args */
    uint32_t reserved;
} ntr_blob;                /* 32 bytes */

enum {
    NTR_BLOB_TRUNCATED = 1,   /* req_size > cap */
    NTR_BLOB_FAULT = 2,       /* read faulted; cap_size bytes are what was readable */
    NTR_BLOB_SAME = 4,        /* after-copy equal to the before-copy: no data stored */
    NTR_BLOB_CHASED = 8,      /* found by pointer scanning, size is a guess */
};

/* NTR_REC_END */
typedef struct ntr_end {
    uint64_t records;
    uint64_t calls;
    uint64_t dropped;      /* records lost (out of memory) */
    uint64_t bytes;
} ntr_end;

#endif
