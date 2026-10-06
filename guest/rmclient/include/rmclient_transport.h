/* SPDX-License-Identifier: MIT */
/*
 * librmclient transport: how RM escapes reach the kernel.
 *
 * RM's user-mode API is a set of numbered escapes (NV_ESC_*) issued on
 * channels: one control channel (Linux: /dev/nvidiactl) plus one channel per
 * GPU (Linux: /dev/nvidiaN, N = the GPU's minor number from
 * NV_ESC_CARD_INFO). librmclient builds the escape payloads (NVOS21/54/00/
 * 33/34/46/47, ...) in their native layout and hands them to the transport,
 * which only has to move bytes.
 *
 * Channels are named by "transport fds": small non-negative ints the
 * transport returns from open(). They are also the values librmclient writes
 * into escape payloads that carry an fd (nv_ioctl_register_fd_t.ctl_fd,
 * nv_ioctl_nvos33_parameters_with_fd.fd, nv_ioctl_alloc_os_event_t.fd), so a
 * transport's fd namespace must be the one its kernel side understands: on
 * Linux these are real file descriptors; a Windows transport going through a
 * Conduit KMD would use handles the KMD resolves the same way.
 *
 * Linux's CPU mapping protocol is: open a fresh channel, NV_ESC_RM_MAP_MEMORY
 * on the control channel naming that channel's fd, mmap() the fresh channel
 * at offset 0, close it. librmclient runs that protocol with open/ioctl/mmap/
 * close below. A transport whose OS maps memory differently (e.g. the KMD
 * maps into the process and returns the address) sets map_memory and
 * unmap_memory instead; librmclient then calls those and skips its own
 * protocol.
 *
 * Everything else a user-mode driver needs from the OS around RM also goes
 * through the transport (ABI 2), so that the driver itself stays portable:
 * waiting for an OS event channel to be signalled (Linux: poll() on the
 * event fd) and allocating the host pages that become
 * NV01_MEMORY_SYSTEM_OS_DESCRIPTOR memory (Linux: anonymous mmap).
 *
 * All callbacks return 0 or a negative errno. Escape status from RM is not a
 * transport error: an escape that reached RM returns 0 and RM's NV_STATUS is
 * in the payload.
 */
#ifndef RMCLIENT_TRANSPORT_H
#define RMCLIENT_TRANSPORT_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* 2 added event_wait, alloc_pages and free_pages at the end of the struct.
 * crm_open still takes ABI 1 transports (those callbacks are then NULL). */
#define CRM_TRANSPORT_ABI 2

/* Channel ("node") to open: the control channel or a GPU minor number. */
#define CRM_NODE_CTL (-1)

/* mmap protection bits. */
#define CRM_PROT_READ  0x1u
#define CRM_PROT_WRITE 0x2u

/* crm_transport.flags */
#define CRM_TRANSPORT_NO_VERSION_CHECK 0x1u /* skip NV_ESC_CHECK_VERSION_STR */
#define CRM_TRANSPORT_NO_REGISTER_FD   0x2u /* skip NV_ESC_REGISTER_FD on GPU channels */

/* What crm_map_memory asks of a transport that maps memory itself. */
struct crm_map_request {
    uint32_t h_client;
    uint32_t h_device;     /* NV01_DEVICE_0 or subdevice */
    uint32_t h_memory;
    uint32_t flags;        /* NVOS33_FLAGS_* */
    uint64_t offset;
    uint64_t length;
    int32_t  node_hint;    /* CRM_NODE_CTL for system memory, else the GPU minor */
};

struct crm_transport {
    uint32_t abi;          /* CRM_TRANSPORT_ABI */
    uint32_t flags;        /* CRM_TRANSPORT_* */
    const char *name;      /* for diagnostics, e.g. "linux" */
    void *ctx;             /* passed back to every callback */
    uint32_t page_size;    /* CPU page size; 0 = 4096 */

    /* Open a channel. node: CRM_NODE_CTL or a GPU minor. *fd >= 0 on success. */
    int  (*open)(void *ctx, int32_t node, int *fd);
    void (*close)(void *ctx, int fd);

    /* Issue escape `nr` (NV_ESC_*, the low 8 bits of NVIDIA's ioctl number)
     * on channel fd with a payload of `size` bytes, read and written in place. */
    int  (*ioctl)(void *ctx, int fd, uint32_t nr, void *arg, uint32_t size);

    /* Map `length` bytes of a channel at `offset` (Linux: always 0 after
     * NV_ESC_RM_MAP_MEMORY), shared. Required unless map_memory is set. */
    int  (*mmap)(void *ctx, int fd, uint64_t offset, uint64_t length,
                 uint32_t prot, void **ptr);
    int  (*munmap)(void *ctx, void *ptr, uint64_t length);

    /* Optional: map an RM memory object by OS-specific means. On success
     * *cpu_ptr is the CPU address of req->offset and *cookie is what RM
     * returned as pLinearAddress (passed back to unmap_memory). */
    int  (*map_memory)(void *ctx, int ctl_fd, const struct crm_map_request *req,
                       void **cpu_ptr, uint64_t *cookie);
    int  (*unmap_memory)(void *ctx, int ctl_fd, const struct crm_map_request *req,
                         void *cpu_ptr, uint64_t cookie);

    /* Optional: called by crm_close after the last channel is closed. */
    void (*destroy)(void *ctx);

    /* ---- ABI 2 ---- */

    /* Optional: block until the event channel fd (from crm_event_open) is
     * signalled or timeout_ms passes. Returns 1 if signalled, 0 on timeout,
     * or a negative errno. NULL: crm_event_wait answers -ENOSYS. */
    int  (*event_wait)(void *ctx, int fd, uint32_t timeout_ms);

    /* Optional: page-aligned, zero-filled, resident host memory that RM can
     * pin as an OS descriptor (crm_alloc_os_descriptor). NULL:
     * crm_alloc_pages answers -ENOSYS. */
    int  (*alloc_pages)(void *ctx, uint64_t size, void **ptr);
    void (*free_pages)(void *ctx, void *ptr, uint64_t size);
};

/* The Linux transport: /dev/nvidiactl, /dev/nvidiaN, ioctl(_IOWR('F', nr,
 * size)), mmap(MAP_SHARED). Device directory defaults to /dev and can be
 * overridden with $CRM_DEV_DIR (tests). NULL on non-Linux builds. */
const struct crm_transport *crm_linux_transport(void);

/* The Windows transport: escapes through the Conduit KMD
 * (HELIOS_ESCAPE_NVRM on D3DKMTEscape). open/close/ioctl, CPU mapping and
 * alloc_pages work; event_wait answers -ENOSYS until the KMD provides OS
 * events. NULL on non-Windows builds. */
const struct crm_transport *crm_windows_transport(void);

/*
 * Windows transport extras: Conduit messages that are not RM escapes, for
 * presenting RM memory through Conduit's zero-copy scanout (docs/SCANOUT.md),
 * the way a Linux guest's KMS does it in its kernel: open one of the host's DRM
 * render nodes, import RM memory there as a GEM object
 * (DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY, after NV0000_CTRL_CMD_OS_UNIX_EXPORT_
 * OBJECT_TO_FD into a fresh control channel), and name that GEM object in a
 * ScanoutFlip. fds are backend handles, as everywhere in this transport, so an
 * fd written into a payload (the export's fd, the import's memFd) is already
 * what the host resolves. Each answers -ENOSYS on non-Windows builds.
 */
#define CRM_WIN_DEV_CTL 255u      /* a fresh control channel (/dev/nvidiactl) */
#define CRM_WIN_DEV_DRI_BASE 512u /* + n: render node n of the host's DRI list */

/* Open a channel of any device type (Open message). *fd >= 0 on success. */
int crm_win_open_device(uint32_t device_type, int *fd);
/* Close what crm_win_open_device opened. */
void crm_win_close_device(int fd);
/* One ioctl with its full Linux number `cmd` (e.g. DRM_IOWR('d', ...)) on fd:
 * `arg` (size bytes) is the top-level struct; `nested` (nested_len bytes, may
 * be NULL) is the block a pointer inside it addresses, sent after it. Both are
 * written back from the reply. Returns 0 or a negative errno (the host's). */
int crm_win_ioctl(int fd, uint32_t cmd, void *arg, uint32_t size, void *nested,
                  uint32_t nested_len);

/* ScanoutFlip (host messages.rs): show GEM object host_handle of the DRM-node
 * file owner_handle (an fd from crm_win_open_device(CRM_WIN_DEV_DRI_BASE + n)). */
struct crm_scanout_flip {
    uint32_t scanout;      /* 0 */
    uint32_t owner_handle;
    uint32_t host_handle;
    uint32_t width, height;
    uint32_t stride;       /* plane 0 pitch, bytes */
    uint32_t offset;       /* plane 0 offset, bytes */
    uint32_t fourcc;       /* DRM_FORMAT_* */
    uint64_t modifier;     /* DRM_FORMAT_MOD_*; 0 = linear */
    uint64_t seq;          /* increasing per flip */
};
/* Returns 0, or a negative errno: the host's verdict, or -EPERM when the KMD
 * does not forward ScanoutFlip (KMD before 22.22.307). */
int crm_win_scanout_flip(const struct crm_scanout_flip *flip);

/*
 * Foreign scanout source (KMD 22.22.308 and later, guest/windows/docs/
 * foreign-scanout.md): instead of sending ScanoutFlips itself, the program
 * registers a source once (layout and the DRM-node file its GEM objects live
 * in) and then names one GEM object per frame. The KMD sends the ScanoutFlip
 * with its own seq and keeps the desktop's flips off scanout 0 while the source
 * is live, and gives the scanout back on release, on close of the DRM file, at
 * process exit, or after lapse_ms without a present. Present from one thread
 * and rotate three or more images (no release event per image yet).
 *
 * Each returns 0 or a negative errno: -ENOSYS when the KMD does not have the
 * op (use crm_win_scanout_flip), -EBUSY when another device's source is live,
 * -ENOENT from present when the source is gone (set it again), -EINVAL for a
 * layout the KMD refuses, -EBADF / -EPERM for the handle.
 */
struct crm_scanout_source {
    uint32_t handle;     /* in:  fd from crm_win_open_device(CRM_WIN_DEV_DRI_BASE + n) */
    uint32_t width, height;
    uint32_t stride;     /* plane 0 pitch, bytes */
    uint32_t offset;     /* plane 0 offset, bytes */
    uint32_t fourcc;     /* DRM_FORMAT_{XRGB,ARGB,XBGR,ABGR}8888 */
    uint64_t modifier;   /* DRM_FORMAT_MOD_*; 0 = linear */
    uint32_t lapse_ms;   /* in:  0 = the KMD's default (2000); out: in effect */
    uint32_t generation; /* out: nonzero source id */
};
int crm_win_scanout_set(struct crm_scanout_source *src);
/* Show GEM object gem of the source's DRM file; *seq (may be NULL) gets the
 * ScanoutFlip's seq. */
int crm_win_scanout_present(uint32_t handle, uint32_t gem, uint64_t *seq);
/* Give scanout 0 back; handle 0 = whatever source this process holds. */
int crm_win_scanout_release(uint32_t handle);

/* The platform default transport (what crm_open(.., NULL) uses). */
const struct crm_transport *crm_default_transport(void);

#ifdef __cplusplus
}
#endif

#endif /* RMCLIENT_TRANSPORT_H */
