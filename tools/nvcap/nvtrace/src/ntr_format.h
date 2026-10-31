/* nvtrace log (.ntr) record format, version 2. Little-endian.
 *
 * File = ntr_file_header, then records back to back. Every record starts with
 * ntr_rec_hdr and its total size is a multiple of 8. Readers skip record
 * types they do not know. Strings are UTF-8, NUL-terminated.
 *
 * Version 2 (memory-mapped writer): a PAD record (type 0) may be as short as
 * 8 bytes (u32 type, u32 size) and is skipped; a zero u32 size ends the data
 * (a log of a killed process keeps a zero-filled tail). Records 9-12 are new.
 * Version 1 logs (queue writer) stay readable.
 */
#ifndef NTR_FORMAT_H
#define NTR_FORMAT_H

#include <stdint.h>

#define NTR_MAGIC "NVTRACE\0"
#define NTR_VERSION 2

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
    /* skip `size` bytes (unused end of a file segment, or a record that was
     * being written when the process died) */
    NTR_REC_PAD = 0,
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
    /* body: ntr_gpumem, then data: GPU-visible memory (command buffers,
     * GPFIFO segments, sampled pages) with its GPU VA */
    NTR_REC_GPUMEM = 9,
    /* body: ntr_region: a CPU mapping learned from a call (Lock, doorbell,
     * fence CPU VA, a pointer returned by an escape or IOCTL) */
    NTR_REC_REGION = 10,
    /* body: ntr_iocomplete, then the output buffer: an IOCTL that returned
     * STATUS_PENDING and completed later */
    NTR_REC_IOCOMPLETE = 11,
    /* body: ntr_handle, then name\0: a device handle found by probing
     * (opened before injection, duplicated or inherited) */
    NTR_REC_HANDLE = 12,
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
    NTR_HOOKERR_CHANGED = 6,  /* the target changed between prepare and commit */
    NTR_HOOKERR_BUSY = 7,     /* a suspended thread sat inside the prologue and could not be moved */
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

/* NTR_REC_GPUMEM */
typedef struct ntr_gpumem {
    uint32_t kind;         /* NTR_GPUMEM_* */
    uint32_t flags;        /* NTR_GM_* */
    uint64_t gpu_va;       /* GPU VA of data[0] (0 if unknown) */
    uint64_t cpu_va;       /* CPU VA it was read from (0 if not mapped) */
    uint32_t req_size;     /* bytes the submission named */
    uint32_t size;         /* bytes stored after this header (then pad to 8) */
    uint32_t hallocation;  /* D3DKMT allocation backing it (0 if unknown) */
    uint32_t call_seq;     /* CALL record this belongs to (0: sampler) */
    uint32_t queue;        /* hHwQueue / hContext of the submission */
    uint32_t index;        /* GPFIFO entry index for segments, page index for samples */
    uint64_t fence;        /* HwQueueProgressFenceId (0 if none) */
    uint64_t aux;          /* SEGMENT: raw GPFIFO entry; SAMPLE: page hash */
} ntr_gpumem;              /* 64 bytes */

enum {
    NTR_GPUMEM_SUBMIT = 1,   /* the command buffer of a SubmitCommand* call */
    NTR_GPUMEM_SEGMENT = 2,  /* a pushbuffer segment named by a GPFIFO entry */
    NTR_GPUMEM_SAMPLE = 3,   /* a changed page of a watched region */
    NTR_GPUMEM_WATCH = 4,    /* a region was added to the sampler (no data) */
};

enum {
    NTR_GM_UNMAPPED = 1,     /* GPU VA not backed by a known CPU mapping: no data */
    NTR_GM_TRUNCATED = 2,
    NTR_GM_FAULT = 4,        /* the read stopped early */
    NTR_GM_FIRST = 8,        /* SAMPLE: first copy of this page */
    NTR_GM_SYNC = 16,        /* SAMPLE: taken at a sync point, not by the timer */
};

/* NTR_REC_REGION */
typedef struct ntr_region {
    uint64_t addr;         /* the address the call returned */
    uint64_t base;         /* VirtualQuery: BaseAddress */
    uint64_t size;         /* bytes from base with the same AllocationBase */
    uint64_t alloc_base;
    uint32_t protect;      /* PAGE_* (WRITECOMBINE 0x400, NOCACHE 0x200) */
    uint32_t type;         /* MEM_IMAGE/MEM_MAPPED/MEM_PRIVATE, 0 if not committed */
    uint32_t source;       /* NTR_RGN_* */
    uint32_t call_seq;     /* CALL record that returned it */
    uint32_t handle;       /* hAllocation / hDoorbell / hSyncObject / hHwQueue */
    uint32_t offset;       /* source ESCAPE/IOCTL: offset of the pointer in the buffer */
} ntr_region;              /* 56 bytes */

enum {
    NTR_RGN_LOCK = 1,
    NTR_RGN_LOCK2 = 2,
    NTR_RGN_SYSMEM = 3,          /* CreateAllocation pSystemMem */
    NTR_RGN_ESCAPE = 4,          /* pointer-like value in escape output */
    NTR_RGN_IOCTL = 5,           /* pointer-like value in IOCTL output */
    NTR_RGN_DOORBELL = 6,
    NTR_RGN_DOORBELL2 = 7,       /* secondary doorbell */
    NTR_RGN_DOORBELL_STATUS = 8,
    NTR_RGN_QUEUED_FENCE = 9,    /* HwQueueProgressFenceLastQueuedValueCPUVirtualAddress */
    NTR_RGN_FENCE = 10,          /* monitored / native fence CPU VA */
    NTR_RGN_HWQUEUE_FENCE = 11,
    NTR_RGN_PAGING_FENCE = 12,
};

/* NTR_REC_IOCOMPLETE */
typedef struct ntr_iocomplete {
    uint32_t call_seq;     /* the NtDeviceIoControlFile/NtFsControlFile CALL */
    uint32_t status;       /* IO_STATUS_BLOCK.Status */
    uint64_t information;  /* IO_STATUS_BLOCK.Information */
    uint64_t handle;
    uint64_t out_addr;
    uint32_t out_len;      /* OutputBufferLength */
    uint32_t size;         /* bytes stored after this header */
    uint32_t flags;        /* NTR_IOC_* */
    uint32_t code;         /* IoControlCode */
} ntr_iocomplete;          /* 48 bytes */

enum {
    NTR_IOC_TIMEOUT = 1,       /* still pending after the timeout; no data */
    NTR_IOC_EARLY = 2,         /* IOSB not STATUS_PENDING when first read */
    NTR_IOC_AT_EXIT = 4,       /* found at log close */
};

/* NTR_REC_HANDLE */
typedef struct ntr_handle {
    uint64_t handle;
    uint32_t how;          /* 1 open (CALL record exists), 2 probe by name */
    uint32_t reserved;
} ntr_handle;

/* NTR_REC_END */
typedef struct ntr_end {
    uint64_t records;
    uint64_t calls;
    uint64_t dropped;      /* records lost (out of memory) */
    uint64_t bytes;
} ntr_end;

#endif
