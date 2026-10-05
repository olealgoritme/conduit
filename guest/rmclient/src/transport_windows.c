/* SPDX-License-Identifier: MIT */
/*
 * librmclient Windows transport: RM escapes through Conduit's display
 * miniport (KMD), as D3DKMTEscape calls on the adapter.
 *
 * STUB. The KMD's escape ABI is not fixed yet, so every call that would
 * reach the kernel answers -ENOSYS and crm_open() fails cleanly with it
 * (NVK then reports no RM device). What the real transport has to do, per
 * callback, once the ABI exists:
 *
 *   open(node)      D3DKMTOpenAdapterFromLuid / -FromGdiDisplayName for the
 *                   Conduit adapter (once, in ctx), then an escape that opens
 *                   a KMD-side channel for CRM_NODE_CTL or a GPU minor and
 *                   returns its id. Ids are the "transport fds": the KMD
 *                   resolves them when they appear inside payloads
 *                   (register_fd.ctl_fd, nvos33_with_fd.fd,
 *                   alloc_os_event.fd, NV0005_ALLOC_PARAMETERS.data).
 *   close(fd)       escape closing that channel.
 *   ioctl(fd,nr,..) D3DKMTEscape with a header { magic, version, fd, nr,
 *                   size } followed by the payload in place; RM status stays
 *                   in the payload. Nested user pointers in payloads
 *                   (NVOS54 params, NVOS21 pAllocParms, NVOS41 pEvent, ...)
 *                   are this process's addresses; the KMD copies them, as
 *                   the Linux guest module does.
 *   map_memory      escape doing NV_ESC_RM_MAP_MEMORY and mapping the result
 *   unmap_memory    into the calling process (MDL / section), returning the
 *                   user address and RM's pLinearAddress cookie. There is no
 *                   mmap on a channel here, so mmap/munmap stay NULL.
 *   event_wait      a Win32 event per event channel: the KMD signals it
 *                   (KeSetEvent on the referenced object) when RM posts the
 *                   OS event; WaitForSingleObject(h, timeout_ms).
 *   alloc_pages     VirtualAlloc: the KMD pins the range with
 *                   MmProbeAndLockPages when it builds the OS descriptor.
 *                   Implemented already.
 */
#include "rmclient_transport.h"

#if defined(_WIN32)

#include <errno.h>
#include <string.h>

#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#include <windows.h>

static int win_open(void *ctx, int32_t node, int *fd)
{
    (void)ctx;
    (void)node;
    *fd = -1;
    return -ENOSYS;
}

static void win_close(void *ctx, int fd)
{
    (void)ctx;
    (void)fd;
}

static int win_ioctl(void *ctx, int fd, uint32_t nr, void *arg, uint32_t size)
{
    (void)ctx;
    (void)fd;
    (void)nr;
    (void)arg;
    (void)size;
    return -ENOSYS;
}

static int win_map_memory(void *ctx, int ctl_fd, const struct crm_map_request *req,
                          void **cpu_ptr, uint64_t *cookie)
{
    (void)ctx;
    (void)ctl_fd;
    (void)req;
    *cpu_ptr = NULL;
    *cookie = 0;
    return -ENOSYS;
}

static int win_unmap_memory(void *ctx, int ctl_fd, const struct crm_map_request *req,
                            void *cpu_ptr, uint64_t cookie)
{
    (void)ctx;
    (void)ctl_fd;
    (void)req;
    (void)cpu_ptr;
    (void)cookie;
    return -ENOSYS;
}

static int win_event_wait(void *ctx, int fd, uint32_t timeout_ms)
{
    (void)ctx;
    (void)fd;
    (void)timeout_ms;
    return -ENOSYS;
}

static int win_alloc_pages(void *ctx, uint64_t size, void **ptr)
{
    (void)ctx;
    if ((uint64_t)(SIZE_T)size != size)
        return -ENOMEM;
    /* Committed pages are zero filled; MEM_COMMIT makes them count against
     * the commit limit now rather than fault later. */
    void *p = VirtualAlloc(NULL, (SIZE_T)size, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE);
    if (!p)
        return -ENOMEM;
    *ptr = p;
    return 0;
}

static void win_free_pages(void *ctx, void *ptr, uint64_t size)
{
    (void)ctx;
    (void)size;
    VirtualFree(ptr, 0, MEM_RELEASE);
}

static struct crm_transport windows_transport = {
    .abi = CRM_TRANSPORT_ABI,
    .flags = 0,
    .name = "windows-stub",
    .ctx = NULL,
    .page_size = 4096,
    .open = win_open,
    .close = win_close,
    .ioctl = win_ioctl,
    .map_memory = win_map_memory,
    .unmap_memory = win_unmap_memory,
    .event_wait = win_event_wait,
    .alloc_pages = win_alloc_pages,
    .free_pages = win_free_pages,
};

const struct crm_transport *crm_windows_transport(void)
{
    return &windows_transport;
}

const struct crm_transport *crm_default_transport(void)
{
    return crm_windows_transport();
}

#else /* !_WIN32 */

const struct crm_transport *crm_windows_transport(void) { return NULL; }

#endif
