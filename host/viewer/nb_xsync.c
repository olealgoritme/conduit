/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * nb_xsync.c -- see nb_xsync.h.
 *
 * The ioctl structures are spelled locally, as nb_gpuscale.c spells EGL: the
 * viewer links no libdrm, and the kernel's own definitions of these are
 * stable ABI.
 */
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <time.h>
#include <unistd.h>

#include "nb_xsync.h"

struct nb_drm_get_cap { uint64_t capability, value; };
struct nb_drm_syncobj_create { uint32_t handle, flags; };
struct nb_drm_syncobj_destroy { uint32_t handle, pad; };
struct nb_drm_syncobj_handle { uint32_t handle, flags; int32_t fd; uint32_t pad; };
struct nb_drm_syncobj_transfer {
    uint32_t src_handle, dst_handle;
    uint64_t src_point, dst_point;
    uint32_t flags, pad;
};
struct nb_drm_syncobj_timeline_array {
    uint64_t handles, points;
    uint32_t count_handles, flags;
};
struct nb_drm_syncobj_eventfd {
    uint32_t handle, flags;
    uint64_t point;
    int32_t  fd;
    uint32_t pad;
};
struct nb_drm_syncobj_timeline_wait {
    uint64_t handles, points;
    int64_t  timeout_nsec;      /* absolute, CLOCK_MONOTONIC                */
    uint32_t count_handles, flags, first_signaled, pad;
};
struct nb_dma_buf_export_sync_file { uint32_t flags; int32_t fd; };

#define NB_DRM_IOWR(nr, t)  _IOWR('d', nr, t)
#define NB_DRM_IOCTL_GET_CAP              NB_DRM_IOWR(0x0c, struct nb_drm_get_cap)
#define NB_DRM_IOCTL_SYNCOBJ_CREATE       NB_DRM_IOWR(0xBF, struct nb_drm_syncobj_create)
#define NB_DRM_IOCTL_SYNCOBJ_DESTROY      NB_DRM_IOWR(0xC0, struct nb_drm_syncobj_destroy)
#define NB_DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD NB_DRM_IOWR(0xC1, struct nb_drm_syncobj_handle)
#define NB_DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE NB_DRM_IOWR(0xC2, struct nb_drm_syncobj_handle)
#define NB_DRM_IOCTL_SYNCOBJ_TL_WAIT      NB_DRM_IOWR(0xCA, struct nb_drm_syncobj_timeline_wait)
#define NB_DRM_IOCTL_SYNCOBJ_QUERY        NB_DRM_IOWR(0xCB, struct nb_drm_syncobj_timeline_array)
#define NB_DRM_IOCTL_SYNCOBJ_TRANSFER     NB_DRM_IOWR(0xCC, struct nb_drm_syncobj_transfer)
#define NB_DRM_IOCTL_SYNCOBJ_TL_SIGNAL    NB_DRM_IOWR(0xCD, struct nb_drm_syncobj_timeline_array)
#define NB_DRM_IOCTL_SYNCOBJ_EVENTFD      NB_DRM_IOWR(0xCF, struct nb_drm_syncobj_eventfd)
#define NB_DMA_BUF_IOCTL_EXPORT_SYNC_FILE _IOWR('b', 2, struct nb_dma_buf_export_sync_file)

#define NB_DRM_CAP_SYNCOBJ_TIMELINE  0x14
#define NB_SYNCOBJ_IMPORT_SYNC_FILE  (1u << 0)
#define NB_DMA_BUF_SYNC_READ         (1u << 0)
#define NB_SYNCOBJ_WAIT_ALL          (1u << 0)
#define NB_SYNCOBJ_WAIT_FOR_SUBMIT   (1u << 1)

struct nb_xs {
    int      fd;                /* the render node                          */
    char     node[64];
    uint32_t tmp;               /* binary syncobj a sync_file passes through */
    int      efd;               /* eventfd for watched points, -1 none      */
    bool     no_export;         /* DMA_BUF_IOCTL_EXPORT_SYNC_FILE missing   */
};

static void say(char *why, size_t n, const char *fmt, ...)
    __attribute__((format(printf, 3, 4)));
static void say(char *why, size_t n, const char *fmt, ...)
{
    va_list ap;

    if (!why || !n) {
        return;
    }
    va_start(ap, fmt);
    vsnprintf(why, n, fmt, ap);
    va_end(ap);
}

static int xioctl(int fd, unsigned long req, void *arg)
{
    int r;

    do {
        r = ioctl(fd, req, arg);
    } while (r < 0 && (errno == EINTR || errno == EAGAIN));
    return r < 0 ? -errno : 0;
}

static bool try_node(struct nb_xs *x, const char *path, char *why, size_t n)
{
    struct nb_drm_get_cap cap = { .capability = NB_DRM_CAP_SYNCOBJ_TIMELINE };
    struct nb_drm_syncobj_create cr = { 0 };
    struct nb_drm_syncobj_eventfd probe = { 0 };
    int fd = open(path, O_RDWR | O_CLOEXEC);

    if (fd < 0) {
        say(why, n, "%s: %s", path, strerror(errno));
        return false;
    }
    if (xioctl(fd, NB_DRM_IOCTL_GET_CAP, &cap) < 0 || !cap.value) {
        say(why, n, "%s has no timeline syncobjs", path);
        close(fd);
        return false;
    }
    if (xioctl(fd, NB_DRM_IOCTL_SYNCOBJ_CREATE, &cr) < 0) {
        say(why, n, "%s: cannot create a syncobj", path);
        close(fd);
        return false;
    }
    x->fd = fd;
    x->tmp = cr.handle;
    snprintf(x->node, sizeof(x->node), "%s", path);
    /*
     * The eventfd watch (Linux 6.6).  Probed on the temporary syncobj with
     * arguments that are valid wherever the ioctl exists, so any error means
     * it does not (an older kernel answers EINVAL) and the tick polls
     * instead.  The probe may fire the eventfd later; a spurious wakeup only
     * costs one query.
     */
    x->efd = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);
    if (x->efd >= 0) {
        probe.handle = x->tmp;
        probe.fd = x->efd;
        if (xioctl(fd, NB_DRM_IOCTL_SYNCOBJ_EVENTFD, &probe) < 0) {
            close(x->efd);
            x->efd = -1;
        }
    }
    return true;
}

/* The render node of the DRM device `dev`: the one whose sysfs device is
 * the same. */
static bool node_of_dev(dev_t dev, char *out, size_t n)
{
    DIR *d = opendir("/dev/dri");
    struct dirent *e;
    bool found = false;
    char b[96], rb[PATH_MAX];   /* realpath() writes up to PATH_MAX bytes */

    if (!d) {
        return false;
    }
    snprintf(b, sizeof(b), "/sys/dev/char/%u:%u/device", major(dev),
             minor(dev));
    if (!realpath(b, rb)) {
        closedir(d);
        return false;
    }
    while (!found && (e = readdir(d))) {
        char path[300], a[96], ra[PATH_MAX];
        struct stat st;

        if (strncmp(e->d_name, "renderD", 7)) {
            continue;
        }
        snprintf(path, sizeof(path), "/dev/dri/%s", e->d_name);
        if (stat(path, &st) < 0) {
            continue;
        }
        snprintf(a, sizeof(a), "/sys/dev/char/%u:%u/device",
                 major(st.st_rdev), minor(st.st_rdev));
        if (realpath(a, ra) && !strcmp(ra, rb)) {
            snprintf(out, n, "%s", path);
            found = true;
        }
    }
    closedir(d);
    return found;
}

struct nb_xs *nb_xs_open(const char *node, dev_t dev, char *why, size_t n)
{
    struct nb_xs *x = calloc(1, sizeof(*x));
    char path[300];

    if (!x) {
        say(why, n, "out of memory");
        return NULL;
    }
    x->fd = -1;
    x->efd = -1;
    if (node && *node && try_node(x, node, why, n)) {
        return x;
    }
    if (dev && node_of_dev(dev, path, sizeof(path)) &&
        try_node(x, path, why, n)) {
        return x;
    }
    for (int i = 128; i < 192; i++) {
        snprintf(path, sizeof(path), "/dev/dri/renderD%d", i);
        if (access(path, F_OK) == 0 && try_node(x, path, why, n)) {
            return x;
        }
    }
    free(x);
    return NULL;
}

void nb_xs_close(struct nb_xs *x)
{
    if (!x) {
        return;
    }
    if (x->fd >= 0) {
        nb_xs_destroy(x, x->tmp);
        close(x->fd);
    }
    if (x->efd >= 0) {
        close(x->efd);
    }
    free(x);
}

const char *nb_xs_node(const struct nb_xs *x)
{
    return x ? x->node : "";
}

int nb_xs_timeline(struct nb_xs *x, uint32_t *handle, int *fd)
{
    struct nb_drm_syncobj_create cr = { 0 };
    struct nb_drm_syncobj_handle h = { 0 };
    int r = xioctl(x->fd, NB_DRM_IOCTL_SYNCOBJ_CREATE, &cr);

    if (r < 0) {
        return r;
    }
    h.handle = cr.handle;
    h.fd = -1;
    r = xioctl(x->fd, NB_DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD, &h);
    if (r < 0) {
        nb_xs_destroy(x, cr.handle);
        return r;
    }
    *handle = cr.handle;
    *fd = h.fd;
    return 0;
}

void nb_xs_destroy(struct nb_xs *x, uint32_t handle)
{
    struct nb_drm_syncobj_destroy d = { .handle = handle };

    if (x && handle) {
        (void)xioctl(x->fd, NB_DRM_IOCTL_SYNCOBJ_DESTROY, &d);
    }
}

int nb_xs_signal(struct nb_xs *x, uint32_t handle, uint64_t point)
{
    struct nb_drm_syncobj_timeline_array a = {
        .handles = (uintptr_t)&handle, .points = (uintptr_t)&point,
        .count_handles = 1,
    };

    return xioctl(x->fd, NB_DRM_IOCTL_SYNCOBJ_TL_SIGNAL, &a);
}

int nb_xs_from_syncfile(struct nb_xs *x, uint32_t handle, uint64_t point,
                        int sync_fd)
{
    /* sync_file -> the temporary binary syncobj -> the timeline point. */
    struct nb_drm_syncobj_handle h = {
        .handle = x->tmp, .flags = NB_SYNCOBJ_IMPORT_SYNC_FILE, .fd = sync_fd,
    };
    struct nb_drm_syncobj_transfer t = {
        .src_handle = x->tmp, .dst_handle = handle, .src_point = 0,
        .dst_point = point,
    };
    int r = xioctl(x->fd, NB_DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE, &h);

    if (r < 0) {
        return r;
    }
    return xioctl(x->fd, NB_DRM_IOCTL_SYNCOBJ_TRANSFER, &t);
}

int nb_xs_from_dmabuf(struct nb_xs *x, uint32_t handle, uint64_t point,
                      int dmabuf)
{
    struct nb_dma_buf_export_sync_file e = {
        .flags = NB_DMA_BUF_SYNC_READ, .fd = -1,
    };
    int r;

    if (!x->no_export) {
        r = xioctl(dmabuf, NB_DMA_BUF_IOCTL_EXPORT_SYNC_FILE, &e);
        if (r == 0) {
            r = nb_xs_from_syncfile(x, handle, point, e.fd);
            close(e.fd);
            return r < 0 ? r : 1;
        }
        if (r == -ENOTTY || r == -EINVAL) {
            x->no_export = true;    /* Linux < 6.0: never again */
        } else {
            return r;
        }
    }
    r = nb_xs_signal(x, handle, point);
    return r < 0 ? r : 0;
}

int nb_xs_query(struct nb_xs *x, const uint32_t *handles, uint64_t *points,
                unsigned n)
{
    struct nb_drm_syncobj_timeline_array a = {
        .handles = (uintptr_t)handles, .points = (uintptr_t)points,
        .count_handles = n,
    };

    if (!n) {
        return 0;
    }
    return xioctl(x->fd, NB_DRM_IOCTL_SYNCOBJ_QUERY, &a);
}

int nb_xs_wait(struct nb_xs *x, uint32_t handle, uint64_t point,
               unsigned timeout_us)
{
    struct timespec ts;
    struct nb_drm_syncobj_timeline_wait wt = {
        .handles = (uintptr_t)&handle, .points = (uintptr_t)&point,
        .count_handles = 1,
        .flags = NB_SYNCOBJ_WAIT_ALL | NB_SYNCOBJ_WAIT_FOR_SUBMIT,
    };

    clock_gettime(CLOCK_MONOTONIC, &ts);
    wt.timeout_nsec = (int64_t)ts.tv_sec * 1000000000ll + ts.tv_nsec +
                      (int64_t)timeout_us * 1000ll;
    return xioctl(x->fd, NB_DRM_IOCTL_SYNCOBJ_TL_WAIT, &wt);
}

int nb_xs_eventfd(const struct nb_xs *x)
{
    return x ? x->efd : -1;
}

int nb_xs_watch(struct nb_xs *x, uint32_t handle, uint64_t point)
{
    struct nb_drm_syncobj_eventfd e = {
        .handle = handle, .point = point, .fd = x->efd,
    };

    if (x->efd < 0) {
        return -ENOTSUP;
    }
    return xioctl(x->fd, NB_DRM_IOCTL_SYNCOBJ_EVENTFD, &e);
}

void nb_xs_drain(struct nb_xs *x)
{
    uint64_t v;

    if (x && x->efd >= 0) {
        while (read(x->efd, &v, sizeof(v)) == (ssize_t)sizeof(v)) {
            /* the counter only says "something fired"; queries say what */
        }
    }
}
