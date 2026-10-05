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

#define CRM_TRANSPORT_ABI 1

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
};

/* The Linux transport: /dev/nvidiactl, /dev/nvidiaN, ioctl(_IOWR('F', nr,
 * size)), mmap(MAP_SHARED). Device directory defaults to /dev and can be
 * overridden with $CRM_DEV_DIR (tests). NULL on non-Linux builds. */
const struct crm_transport *crm_linux_transport(void);

/* The platform default transport (what crm_open(.., NULL) uses). */
const struct crm_transport *crm_default_transport(void);

#ifdef __cplusplus
}
#endif

#endif /* RMCLIENT_TRANSPORT_H */
