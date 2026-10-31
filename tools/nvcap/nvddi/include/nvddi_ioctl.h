/* Control interface of nvddi.sys: \\.\NvDdiCtl (administrators only). */
#pragma once

#define NVDDI_CTL_DEVICE   L"\\Device\\NvDdiCtl"
#define NVDDI_CTL_LINK     L"\\DosDevices\\NvDdiCtl"
#define NVDDI_CTL_WIN32    L"\\\\.\\NvDdiCtl"

#define NVDDI_IOCTL(n) CTL_CODE(FILE_DEVICE_UNKNOWN, 0x900 + (n), METHOD_BUFFERED, FILE_ANY_ACCESS)

#define IOCTL_NVDDI_STATUS     NVDDI_IOCTL(0)   /* out: NVDDI_STATUS */
#define IOCTL_NVDDI_START      NVDDI_IOCTL(1)   /* open a new log file and capture */
#define IOCTL_NVDDI_STOP       NVDDI_IOCTL(2)   /* stop capture, flush and close the file */
#define IOCTL_NVDDI_FLUSH      NVDDI_IOCTL(3)   /* drain the rings to the file now */
#define IOCTL_NVDDI_SET_LEVEL  NVDDI_IOCTL(4)   /* in: ULONG level 0..3 */
#define IOCTL_NVDDI_MUTE       NVDDI_IOCTL(5)   /* in: NVDDI_MUTE */
#define IOCTL_NVDDI_MARK       NVDDI_IOCTL(6)   /* in: ASCII text, logged as an event */

#define NVDDI_LEVEL_OFF     0   /* wrappers stay installed, nothing recorded */
#define NVDDI_LEVEL_CALLS   1   /* calls, argument values, return values */
#define NVDDI_LEVEL_STRUCTS 2   /* + argument structures */
#define NVDDI_LEVEL_FULL    3   /* + nested buffers (private data, DMA, lists, ...) */

#pragma pack(push, 8)
typedef struct NVDDI_STATUS {
    unsigned int       version;          /* 0xMMmmpppp */
    unsigned int       level;
    unsigned int       capturing;        /* file open and being written */
    unsigned int       attached;         /* filter on \Device\DxgKrnl */
    unsigned int       registrations;    /* registration IOCTLs seen */
    unsigned int       hooked;           /* miniports with wrapped tables */
    unsigned int       ncpu;
    unsigned int       ring_bytes;
    unsigned long long seq;              /* records produced */
    unsigned long long dropped;          /* records dropped (rings) */
    unsigned long long dropped_file;     /* records dropped (file cap / error) */
    unsigned long long file_bytes;
    unsigned long long calls;            /* wrapped calls seen (any level) */
    unsigned int       fail_count;       /* Parameters\FailCount at load */
    unsigned int       last_error;
    unsigned short     file_name[200];   /* current/last file (NT path) */
    char               hooked_driver[64];
} NVDDI_STATUS;

typedef struct NVDDI_MUTE {
    unsigned int       mute;             /* 1 mute, 0 unmute */
    char               name[124];        /* slot name (e.g. DxgkDdiInterruptRoutine) or "*" */
} NVDDI_MUTE;
#pragma pack(pop)
