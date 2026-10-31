/* nvddi.sys - WDDM DDI tracer between dxgkrnl.sys and a display miniport. */
#pragma once

#include "wdkshim.h"
#include <ntddk.h>
#include <dispmprt.h>
#include <d3dkmddi.h>
#include <stdarg.h>

#include "ndl_format.h"
#include "nvddi_ioctl.h"
#include "nvddi_gen.h"

/* kernel exports the mingw-w64 ddk headers do not declare */
typedef struct _NVDDI_MM_COPY_ADDRESS {
    union {
        PVOID            VirtualAddress;
        PHYSICAL_ADDRESS PhysicalAddress;
    };
} MM_COPY_ADDRESS;
#define MM_COPY_MEMORY_PHYSICAL 0x1
#define MM_COPY_MEMORY_VIRTUAL  0x2
NTKERNELAPI NTSTATUS NTAPI MmCopyMemory(PVOID TargetAddress, MM_COPY_ADDRESS SourceAddress, SIZE_T NumberOfBytes,
                                        ULONG Flags, PSIZE_T NumberOfBytesTransferred);
NTKERNELAPI BOOLEAN NTAPI SeTokenIsAdmin(PACCESS_TOKEN Token);
NTKERNELAPI PVOID NTAPI MmMapIoSpaceEx(PHYSICAL_ADDRESS PhysicalAddress, SIZE_T NumberOfBytes, ULONG Protect);
NTKERNELAPI PDEVICE_OBJECT NTAPI IoGetDeviceAttachmentBaseRef(PDEVICE_OBJECT DeviceObject);
NTSYSAPI NTSTATUS NTAPI ZwFlushBuffersFile(HANDLE FileHandle, PIO_STATUS_BLOCK IoStatusBlock);
#ifndef SeQuerySubjectContextToken
#define SeQuerySubjectContextToken(sc) \
    ((sc)->ClientToken ? (sc)->ClientToken : (sc)->PrimaryToken)
#endif
__declspec(dllimport) extern ULONG InitSafeBootMode;   /* nonzero in safe mode */

#define NVDDI_VERSION   0x00010000u          /* 1.0.0 */
#define NVDDI_TAG       0x6964644E           /* 'Nddi' */
#define NVDDI_WRAPCC                         /* x64 has one calling convention */
#define NVDDI_MAX_ARGS  12
#define NVDDI_MAX_SAVED 8

#define ALIGN8(x) (((x) + 7u) & ~7u)

/* ------------------------------------------------------------ generated tables */

/* argument kinds (gen.py AK) */
#define AK_VAL     0   /* value only */
#define AK_PREC    1   /* pointer to a record */
#define AK_PSCALAR 2   /* pointer to a scalar or pointer */
#define AK_PVOID   3   /* void pointer (captured only with a length argument) */
#define AK_PPREC   4   /* pointer to a pointer to a record (out) */
#define AK_BYVAL   5   /* passed by value, not a scalar (struct <= 8 bytes, float) */
#define AK_PPTABLE 6   /* out pointer to a function table: wrapped after the call */
#define AK_PTABLE  7   /* pointer to a function table: replaced before the call */

#define AF_CONST 1     /* pointee is const: no POST capture */
#define AF_BUF   2     /* length in argument 'lenarg' (bytes) */
#define AF_POSTRULES 4 /* const pointee, but its rules capture buffers after the call */
#define AF_SIZE16    8 /* pointee starts with a USHORT Size (INTERFACE): capture that many bytes */

typedef struct NVDDI_ARG {
    UCHAR  kind;
    UCHAR  flags;
    USHORT rs;          /* rule set of the pointee record, 0xFFFF none */
    UCHAR  lenarg;      /* 0xFF none */
    ULONG  size;        /* pointee size */
    UCHAR  table;       /* AK_PTABLE/AK_PPTABLE: table id */
} NVDDI_ARG;

typedef struct NVDDI_SLOT {
    UCHAR       table;
    USHORT      index;
    UCHAR       nargs;
    UCHAR       retkind;    /* 0 void, 1 scalar, 2 other */
    UCHAR       retsize;
    USHORT      field_off;  /* offset of the slot in its table */
    void       *wrapper;
    const char *name;
    NVDDI_ARG   a[NVDDI_MAX_ARGS];
} NVDDI_SLOT;

#define TH_DDI    0   /* DRIVER_INITIALIZATION_DATA: copied at registration */
#define TH_SIZE   1   /* first ULONG is the size of the table */
#define TH_STATIC 2   /* fixed size, shared static table in dxgkrnl */

typedef struct NVDDI_TABLE {
    UCHAR             id;
    USHORT            nslots;
    UCHAR             how;      /* TH_* */
    ULONG             size;     /* sizeof the C type */
    ULONG             maxver;   /* highest Version field covered, 0 = no check */
    const NVDDI_SLOT *slots;
    void            **orig;
    const char       *name;
} NVDDI_TABLE;

/* rule kinds (gen.py RK) */
#define RK_BYTES    1
#define RK_COUNT    2
#define RK_ONE      3
#define RK_PTRARRAY 4
#define RK_WRITTEN  5
#define RK_RANGE    6
#define RK_MDL      7
#define RK_ADL      8
#define RK_PRODUCT  9

#define W_PRE  1
#define W_POST 2

typedef struct NVDDI_RULE {
    USHORT id;
    UCHAR  kind;
    UCHAR  when;
    USHORT ptr_off;
    USHORT len_off;
    UCHAR  len_size;
    UCHAR  end_size;
    USHORT end_off;
    USHORT sub;         /* rule set for elements, 0xFFFF none */
    USHORT mult;
    ULONG  elem_size;
    ULONG  maxcount;
    USHORT cond_off;
    UCHAR  cond_size;   /* 0 = no condition */
    ULONG  cond_val;
    USHORT cond2_off;
    UCHAR  cond2_size;
    ULONG  cond2_val;
} NVDDI_RULE;

typedef struct NVDDI_RULESET {
    USHORT first;
    USHORT n;
    ULONG  size;        /* sizeof the record */
} NVDDI_RULESET;

extern const NVDDI_RULE    g_nvddi_rules[];
extern const NVDDI_RULESET g_nvddi_rulesets[];
extern const ULONG         g_nvddi_nrulesets;
extern const NVDDI_TABLE   g_nvddi_tables[NVDDI_NTABLES];
extern const char          g_nvddi_schema[];
extern const ULONG         g_nvddi_schema_len;

/* ------------------------------------------------------------ per call state */

typedef struct NVDDI_CALL {
    const NVDDI_SLOT *s;
    ULONG64           seq;          /* seq of the CALL record, 0 if not recorded */
    UCHAR             active;
    UCHAR             irql;
    UCHAR             nsaved;
    UCHAR             level;
    ULONG_PTR         pre[NVDDI_MAX_ARGS];      /* argument values as received */
    ULONG64           saved[NVDDI_MAX_SAVED];   /* RK_WRITTEN: pointer before the call */
    USHORT            saved_rule[NVDDI_MAX_SAVED];
} NVDDI_CALL;

void nvddi_pre(NVDDI_CALL *c, const NVDDI_SLOT *s, ULONG_PTR *av);
void nvddi_post(NVDDI_CALL *c, ULONG_PTR *av, ULONG64 ret);

/* ------------------------------------------------------------ rings */

typedef struct NVDDI_RING {
    UCHAR           *buf;
    ULONG            size;          /* power of two */
    ULONG            mask;
    volatile LONG64  head;          /* reservation position (monotonic) */
    volatile LONG64  tail;          /* consumer position (monotonic) */
    volatile LONG64  dropped;       /* records */
    volatile LONG64  dropped_bytes;
    LONG64           reported_dropped;
    LONG64           reported_dropped_bytes;
} NVDDI_RING;

/* reserve a record of 'size' bytes (multiple of 8); NULL when full */
NDL_REC *ring_reserve(ULONG size, USHORT type);
void     ring_commit(NDL_REC *r);
NTSTATUS ring_init(ULONG ring_bytes);

/* convenience records (any IRQL <= DISPATCH unless noted) */
void log_event(ULONG code, NTSTATUS status, ULONG64 a, ULONG64 b, const char *fmt, ...);

/* ------------------------------------------------------------ state */

#define ST_PENDING  0   /* no file yet (boot): keep records in the rings */
#define ST_CAPTURE  1   /* file open */
#define ST_STOPPED  2   /* stopped by IOCTL: records are not produced */

#define WCMD_START  1
#define WCMD_STOP   2
#define WCMD_FLUSH  4
#define WCMD_EXIT   8

typedef struct NVDDI_GLOBALS {
    PDRIVER_OBJECT   driver;
    UNICODE_STRING   regpath;           /* service key, NUL-terminated copy */
    PDEVICE_OBJECT   ctl;
    PDEVICE_OBJECT   filter;
    PDEVICE_OBJECT   lower;
    PDRIVER_OBJECT   dxgkrnl;           /* bottom driver of \Device\DxgKrnl */

    /* configuration (Parameters key) */
    ULONG            enabled, cfg_level, ring_kb, total_ring_mb, max_blob, max_file_mb;
    ULONG            max_fails, fail_count, boot_count, healthy_seconds;
    ULONG            hook_callbacks, hook_vidpn, log_registry, resolve_gpuva, hook_imports, gpuva_entries;
    ULONG            boot_start;        /* wrap nvlddmkm's first start (load order gate) */
    WCHAR            logdir[128];
    WCHAR            target[64];

    volatile LONG    level;
    volatile LONG    state;             /* ST_* */
    volatile LONG    attached;
    volatile LONG    registrations;
    volatile LONG    hooked;
    volatile LONG    last_error;
    volatile LONG    start_failed;      /* hooked StartDevice failed: stay disabled */

    /* rings */
    NVDDI_RING      *rings;
    ULONG            ncpu;
    ULONG            ring_bytes;
    volatile LONG    rings_ready;
    volatile LONG64  seq;
    volatile LONG64  calls;

    /* writer */
    PETHREAD         writer;
    KEVENT           wake;
    KEVENT           cmd_done;
    volatile LONG    cmd;
    volatile LONG64  file_bytes;
    volatile LONG64  dropped_file;
    WCHAR            file_name[200];
    ULONG            file_index;

    /* hooked miniport */
    void            *real_dpi;
    PDRIVER_OBJECT   hooked_drv;
    char             hooked_name[64];
    volatile PVOID   tgt_base;          /* hooked miniport image, for registry stack checks */
    ULONG            tgt_size;
    KSPIN_LOCK       table_lock;

    /* mute bitmaps: table x slot */
    volatile LONG    mute[NVDDI_NTABLES][8];

    LARGE_INTEGER    qpc_freq;
    ULONG64          load_qpc;
} NVDDI_GLOBALS;

extern NVDDI_GLOBALS g;

/* main.c */
NTSTATUS cfg_write_dword(const WCHAR *name, ULONG v);
void     nvddi_set_fail_count(ULONG v);
BOOLEAN  slot_muted(const NVDDI_SLOT *s);
ULONG    mute_by_name(const char *name, ULONG mute);

/* filter.c */
NTSTATUS filter_attach(void);
VOID filter_reinit(PDRIVER_OBJECT drv, PVOID ctx, ULONG count);
NTSTATUS filter_dispatch(PDEVICE_OBJECT dev, PIRP irp);
void    *wrap_table(ULONG tid, void *src);

/* registry.c */
NTSTATUS registry_init(void);

/* gpuva.c: GPU VA -> command bytes */
NTSTATUS gpuva_init(void);
void gpuva_note_segment(ULONG id, ULONG flags, ULONG64 base, ULONG64 cpu_translated, ULONG64 size);
void gpuva_note_pte(ULONG64 va, ULONG64 pte_flags, ULONG64 pte_pageaddr);
ULONG gpuva_capture(ULONG64 owner_seq, ULONG64 va, ULONG size, ULONG engine);
ULONG gpuva_pte_count(void);

/* imports.c: non-DDI kernel imports of the miniport */
NTSTATUS imports_wrap(PDRIVER_OBJECT drv);
void imports_unwrap(void);

/* writer.c */
NTSTATUS writer_start(void);
NTSTATUS writer_command(LONG cmd, ULONG timeout_ms);

/* util.c */
int      fmt_v(char *buf, ULONG size, const char *f, va_list ap);
int      fmt(char *buf, ULONG size, const char *f, ...);
ULONG    safe_copy(void *dst, ULONG_PTR src, ULONG len, KIRQL irql, BOOLEAN direct_ok, USHORT *flags);
BOOLEAN  is_kernel_address(ULONG_PTR p);
ULONG    cpu_index(void);
int      str_ieq_w(const WCHAR *a, const WCHAR *b);
