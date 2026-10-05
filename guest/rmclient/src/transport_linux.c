/* SPDX-License-Identifier: MIT */
/*
 * librmclient Linux transport: NVIDIA's character devices.
 *
 *   /dev/nvidiactl   control channel (CRM_NODE_CTL)
 *   /dev/nvidiaN     GPU channel, N = minor number from NV_ESC_CARD_INFO
 *
 * Escapes are ioctl(fd, _IOWR('F', nr, size), arg), as libnvidia issues them.
 * Under Conduit these nodes come from the guest module conduit_gpu, which
 * forwards each ioctl and mmap to the host's driver unchanged.
 */
#define _GNU_SOURCE
#include "rmclient_transport.h"

#if defined(__linux__)

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <unistd.h>

#include "nv_ioctl_defs.h"

static const char *dev_dir(void)
{
    const char *d = getenv("CRM_DEV_DIR");
    return (d && *d) ? d : "/dev";
}

static int linux_open(void *ctx, int32_t node, int *fd)
{
    char path[256];
    (void)ctx;
    if (node == CRM_NODE_CTL)
        snprintf(path, sizeof(path), "%s/nvidiactl", dev_dir());
    else if (node >= 0 && node < 255)
        snprintf(path, sizeof(path), "%s/nvidia%d", dev_dir(), (int)node);
    else
        return -EINVAL;

    int f;
    do {
        f = open(path, O_RDWR | O_CLOEXEC);
    } while (f < 0 && errno == EINTR);
    if (f < 0)
        return -errno;
    *fd = f;
    return 0;
}

static void linux_close(void *ctx, int fd)
{
    (void)ctx;
    if (fd >= 0)
        close(fd);
}

static int linux_ioctl(void *ctx, int fd, uint32_t nr, void *arg, uint32_t size)
{
    (void)ctx;
    if (size > _IOC_SIZEMASK)
        return -E2BIG; /* would need NV_ESC_IOCTL_XFER_CMD; nothing here is that big */
    unsigned long req = _IOC(_IOC_READ | _IOC_WRITE, NV_IOCTL_MAGIC, nr & 0xff, size);
    int r;
    do {
        r = ioctl(fd, req, arg);
    } while (r < 0 && errno == EINTR);
    return r < 0 ? -errno : 0;
}

static int linux_mmap(void *ctx, int fd, uint64_t offset, uint64_t length,
                      uint32_t prot, void **ptr)
{
    (void)ctx;
    int p = 0;
    if (prot & CRM_PROT_READ)
        p |= PROT_READ;
    if (prot & CRM_PROT_WRITE)
        p |= PROT_WRITE;
    void *m = mmap(NULL, (size_t)length, p, MAP_SHARED, fd, (off_t)offset);
    if (m == MAP_FAILED)
        return -errno;
    *ptr = m;
    return 0;
}

static int linux_munmap(void *ctx, void *ptr, uint64_t length)
{
    (void)ctx;
    return munmap(ptr, (size_t)length) < 0 ? -errno : 0;
}

static struct crm_transport linux_transport = {
    .abi = CRM_TRANSPORT_ABI,
    .flags = 0,
    .name = "linux",
    .ctx = NULL,
    .page_size = 0, /* set from sysconf() on first use */
    .open = linux_open,
    .close = linux_close,
    .ioctl = linux_ioctl,
    .mmap = linux_mmap,
    .munmap = linux_munmap,
};

const struct crm_transport *crm_linux_transport(void)
{
    if (linux_transport.page_size == 0) {
        long ps = sysconf(_SC_PAGESIZE);
        linux_transport.page_size = ps > 0 ? (uint32_t)ps : 4096u;
    }
    return &linux_transport;
}

const struct crm_transport *crm_default_transport(void)
{
    return crm_linux_transport();
}

#else /* !__linux__ */

const struct crm_transport *crm_linux_transport(void) { return NULL; }
const struct crm_transport *crm_default_transport(void) { return NULL; }

#endif
