/*
 * NDL: nvddi log format, version 1.  Little endian, all records 8-byte aligned.
 *
 * File = NDL_FILE_HEADER, then records back to back.  Records from different
 * CPUs are interleaved in blocks (one block per CPU ring per writer pass);
 * order by NDL_REC.seq to get the global order.  decoder/ndldump.py mirrors
 * these definitions.
 *
 * Record types
 *   NDL_T_SCHEMA    text: names/kinds of every wrapped table, slot, argument
 *                   and capture rule (generated from the WDK headers)
 *   NDL_T_EVENT     driver event: u32 code, u32 status, text
 *   NDL_T_IRP       IRP that passed the \Device\DxgKrnl filter
 *   NDL_T_REGISTER  a miniport registered with dxgkrnl (DxgkInitialize)
 *   NDL_T_TABLE     a function table was wrapped (raw copy of the original)
 *   NDL_T_CALL      a wrapped function was entered: arguments + PRE blobs
 *   NDL_T_RET       it returned: return value, arguments again, POST blobs
 *   NDL_T_BLOB      one nested buffer of a CALL/RET (linked by owner_seq)
 *   NDL_T_DROP      records lost since the last DROP (ring full / file cap)
 *   NDL_T_END       written when the log is closed
 *   NDL_T_REGISTRY  registry access made from the hooked miniport's code
 *   NDL_T_IMPORT    a logged ntoskrnl/hal import call made from the miniport
 */
#pragma once

#define NDL_MAGIC        "NVDDINDL"
#define NDL_VERSION      1
#define NDL_HEADER_SIZE  256

#define NDL_T_PAD       0
#define NDL_T_SCHEMA    1
#define NDL_T_EVENT     2
#define NDL_T_IRP       3
#define NDL_T_REGISTER  4
#define NDL_T_TABLE     5
#define NDL_T_CALL      6
#define NDL_T_RET       7
#define NDL_T_BLOB      8
#define NDL_T_DROP      9
#define NDL_T_END       10
#define NDL_T_REGISTRY  11
#define NDL_T_IMPORT    12

/* NDL_REC.flags */
#define NDL_RF_HIGHIRQL  0x0001   /* recorded above DISPATCH_LEVEL: no nested capture */

#pragma pack(push, 8)

typedef struct NDL_FILE_HEADER {
    char               magic[8];          /* "NVDDINDL" */
    unsigned int       version;           /* NDL_VERSION */
    unsigned int       header_size;       /* NDL_HEADER_SIZE */
    unsigned long long qpc_freq;          /* KeQueryPerformanceCounter frequency */
    unsigned long long start_qpc;         /* QPC when this file was opened */
    unsigned long long start_systime;     /* FILETIME (UTC) when this file was opened */
    unsigned long long load_qpc;          /* QPC at DriverEntry */
    unsigned int       boot_count;        /* Parameters\BootCount at DriverEntry */
    unsigned int       file_index;        /* 0 for the first file of this boot */
    unsigned int       ncpu;              /* rings */
    unsigned int       ring_bytes;        /* per ring */
    unsigned int       os_major, os_minor, os_build;
    unsigned int       level;             /* capture level at open */
    unsigned int       max_blob;          /* bytes */
    unsigned int       nvddi_version;     /* 0xMMmmpppp */
    unsigned int       wdk_ddi_version;   /* DXGKDDI_INTERFACE_VERSION of the headers */
    unsigned int       reserved0;
    unsigned char      reserved[256 - 96];
} NDL_FILE_HEADER;

typedef struct NDL_REC {                  /* 48 bytes */
    unsigned int       size;              /* whole record incl. this header, multiple of 8 */
    unsigned short     type;              /* NDL_T_* */
    unsigned short     flags;             /* NDL_RF_* */
    unsigned long long seq;               /* global sequence number, 1-based */
    unsigned long long qpc;
    unsigned long long thread;            /* KTHREAD address */
    unsigned int       pid, tid;          /* 0 when recorded above DISPATCH_LEVEL */
    unsigned short     cpu;               /* processor index */
    unsigned char      irql;
    unsigned char      rsv0;
    unsigned int       rsv1;
} NDL_REC;

/* payload of NDL_T_CALL and NDL_T_RET, after NDL_REC */
typedef struct NDL_CALL {
    unsigned short     table;             /* schema T id */
    unsigned short     slot;              /* schema S index */
    unsigned char      nargs;
    unsigned char      phase;             /* 0 = CALL, 1 = RET */
    unsigned short     nblobs;            /* NDL_BLOBs that follow the args */
    unsigned long long call_seq;          /* RET: seq of the CALL (0 if it was dropped) */
    unsigned long long ret;               /* RET: return value (rax / first 8 bytes) */
    /* unsigned long long args[nargs]; then nblobs x (NDL_BLOB + data) */
} NDL_CALL;

/* blob kinds */
#define NDL_B_ARG     1   /* pointee of argument 'arg' */
#define NDL_B_NEST    2   /* rule 'rule' applied to the parent (arg pointee or element) */
#define NDL_B_ELEM    3   /* ptrarray: element 'elem' pointee */
#define NDL_B_LENBUF  4   /* void* argument with a length argument */
#define NDL_B_TABLE   5   /* raw function table copy */
#define NDL_B_INIT    6   /* DRIVER_INITIALIZATION_DATA as passed by the miniport */
#define NDL_B_CMDBUF  7   /* command bytes at a submitted GPU VA (arg=engine, rule=segment, elem=run) */

/* blob flags */
#define NDL_BF_POST     0x0001  /* captured after the call */
#define NDL_BF_TRUNC    0x0002  /* orig_len > len: capped */
#define NDL_BF_PARTIAL  0x0004  /* copy failed part-way; rest zero */
#define NDL_BF_FAILED   0x0008  /* copy failed entirely */
#define NDL_BF_IRQL     0x0010  /* skipped: IRQL too high for this source */
#define NDL_BF_DIRECT   0x0020  /* copied with a direct read after MmCopyMemory refused */
#define NDL_BF_USER     0x0040  /* source address was a user-mode address */

typedef struct NDL_BLOB {                 /* 32 bytes, data follows padded to 8 */
    unsigned int       len;               /* bytes stored */
    unsigned int       orig_len;          /* bytes the rule asked for */
    unsigned short     kind;              /* NDL_B_* */
    unsigned short     arg;               /* argument index */
    unsigned short     rule;              /* rule id (NEST/ELEM), 0xFFFF otherwise */
    unsigned short     flags;             /* NDL_BF_* */
    unsigned int       elem;              /* element index in the parent array, 0xFFFFFFFF if none */
    unsigned short     parent_rule;       /* rule that produced the parent array, 0xFFFF = arg pointee */
    unsigned short     depth;
    unsigned long long addr;              /* source address */
} NDL_BLOB;

/* NDL_T_BLOB payload: u64 owner_seq (seq of the CALL or RET record it belongs to),
   then one NDL_BLOB + data */

typedef struct NDL_EVENT {
    unsigned int       code;              /* NDL_EV_* */
    unsigned int       status;
    unsigned long long a, b;
    /* char text[] NUL-terminated, padded */
} NDL_EVENT;

#define NDL_EV_LOAD         1   /* text: config */
#define NDL_EV_ATTACH       2   /* attached to \Device\DxgKrnl; a = lower device */
#define NDL_EV_ATTACH_FAIL  3
#define NDL_EV_REGISTER_IOCTL 4 /* registration IOCTL seen; a = DpiInitialize, b = code */
#define NDL_EV_HOOK_SKIP    5   /* miniport not hooked; text = reason */
#define NDL_EV_TABLE_SKIP   6   /* table not wrapped; a = table id; text = reason */
#define NDL_EV_START_FAIL   7   /* hooked StartDevice failed; status */
#define NDL_EV_LEVEL        8   /* capture level changed; a = level */
#define NDL_EV_FILE         9   /* file opened; text = path */
#define NDL_EV_HEALTHY      10  /* fail counter cleared */
#define NDL_EV_INFO         11

typedef struct NDL_IRP {
    unsigned char      major, minor, requestor_mode, rsv;
    unsigned int       ioctl;
    unsigned int       in_len, out_len;
    unsigned int       status;            /* 0x103 if passed down without waiting */
    unsigned int       rsv2;
    unsigned long long file_object;
    unsigned long long user_buffer;
} NDL_IRP;

typedef struct NDL_REGISTER {
    unsigned long long driver_object;
    unsigned long long image_base;        /* DriverObject->DriverStart */
    unsigned int       image_size;
    unsigned int       init_version;      /* DRIVER_INITIALIZATION_DATA.Version */
    unsigned int       hooked;            /* 1 if this miniport's tables are wrapped */
    unsigned int       status;            /* DpiInitialize result */
    unsigned long long dpi_initialize;
    unsigned short     name_len;          /* bytes of UTF-16 driver name that follow */
    unsigned short     regpath_len;       /* bytes of UTF-16 registry path after the name */
    unsigned int       nblobs;
    /* WCHAR name[], WCHAR regpath[], pad to 8, then blobs (NDL_B_INIT) */
} NDL_REGISTER;

typedef struct NDL_TABLE {
    unsigned int       table;             /* schema T id */
    unsigned int       wrapped;           /* number of slots replaced */
    unsigned long long source;            /* original table address */
    unsigned long long copy;              /* wrapped copy handed out */
    unsigned int       size;              /* bytes copied */
    unsigned int       version;           /* table Version field, 0 if none */
    /* NDL_BLOB (NDL_B_TABLE) + raw original table */
} NDL_TABLE;

typedef struct NDL_DROP {
    unsigned long long records;
    unsigned long long bytes;
    unsigned int       cpu;
    unsigned int       reason;            /* 1 ring full, 2 file size cap reached (rest of the capture dropped) */
} NDL_DROP;

typedef struct NDL_IMPORT {
    unsigned int       fn;                /* NDL_IMP_* */
    unsigned int       rsv;
    unsigned long long a, b, c, d;        /* call arguments (meaning per fn, see the decoder) */
    unsigned long long ret;               /* return value / out handle */
    /* optional trailing NDL_BLOB (e.g. value name for a Zw registry call) */
} NDL_IMPORT;

#define NDL_IMP_MmMapIoSpaceEx            1   /* a=phys b=len c=prot ret=va */
#define NDL_IMP_MmMapIoSpace              2
#define NDL_IMP_MmUnmapIoSpace            3   /* a=va b=len */
#define NDL_IMP_MmAllocateContiguousMemorySpecifyCache 4 /* a=bytes b=lowest c=highest ret=va */
#define NDL_IMP_MmAllocateContiguousMemory 5  /* a=bytes b=highest ret=va */
#define NDL_IMP_MmFreeContiguousMemory    6   /* a=va */
#define NDL_IMP_MmAllocatePagesForMdlEx   7   /* a=low b=high c=bytes ret=mdl */
#define NDL_IMP_MmMapLockedPagesSpecifyCache 8 /* a=mdl ret=va */
#define NDL_IMP_IoAllocateMdl             9   /* a=va b=len ret=mdl */
#define NDL_IMP_MmProbeAndLockPages       10  /* a=mdl */
#define NDL_IMP_KeInsertQueueDpc          11  /* a=dpc b=arg1 c=arg2 ret=inserted */
#define NDL_IMP_IoConnectInterrupt        12  /* a=service-routine b=context ret=status */
#define NDL_IMP_IoConnectInterruptEx      13  /* a=params ret=status */
#define NDL_IMP_ExAllocatePool2           14  /* a=flags b=bytes c=tag ret=ptr */
#define NDL_IMP_ExAllocatePoolWithTag     15  /* a=type b=bytes c=tag ret=ptr */
#define NDL_IMP_MmGetPhysicalAddress      16  /* a=va ret=phys */
#define NDL_IMP_ZwOpenKey                 17  /* blob=key name; ret=status */
#define NDL_IMP_ZwQueryValueKey           18  /* blob=value name; a=class ret=status */
#define NDL_IMP_ZwCreateFile              19  /* blob=object name; ret=status */

typedef struct NDL_REGISTRY {
    unsigned int       op;                /* REG_NOTIFY_CLASS (RegNtPostQueryValueKey, ...) */
    unsigned int       status;            /* NTSTATUS of the operation; 0x103 for PRE callbacks */
    unsigned int       type;              /* SetValue: REG_* type */
    unsigned int       info_class;        /* Query/Enumerate: KEY_VALUE_INFORMATION_CLASS */
    unsigned int       index;             /* EnumerateValueKey index */
    unsigned short     key_len;           /* bytes of UTF-16 key path that follow */
    unsigned short     value_len;         /* bytes of UTF-16 value name after the key path */
    unsigned int       data_len;          /* bytes of data stored (after the names, 8-aligned) */
    unsigned int       data_orig;         /* bytes the operation transferred */
    unsigned short     flags;             /* NDL_BF_* */
    unsigned short     rsv;
    /* WCHAR key[], WCHAR value[], pad to 8, data[] (KEY_VALUE_*_INFORMATION or raw value) */
} NDL_REGISTRY;

typedef struct NDL_END {
    unsigned long long records;
    unsigned long long bytes;
    unsigned long long dropped;
} NDL_END;

#pragma pack(pop)
