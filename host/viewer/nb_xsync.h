/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * nb_xsync.h -- DRM syncobj timelines for explicit sync with the compositor
 * (wp_linux_drm_syncobj_surface_v1).
 *
 * What a commit carries with explicit sync: an ACQUIRE point the compositor
 * waits for before it reads the buffer, and a RELEASE point it signals when
 * it is done with it.  This file makes and drives those timelines with plain
 * DRM ioctls on a render node (no libdrm, like the rest of the viewer):
 *
 *   acquire  a zero-copy frame: the dma-buf's own pending writes, exported
 *            as a sync_file (DMA_BUF_IOCTL_EXPORT_SYNC_FILE) and put on the
 *            timeline point; a kernel without that ioctl gets the point
 *            signalled from the CPU, which is right for a frame the guest
 *            finished before it was handed over (docs/SYNC.md: the guest's
 *            KMS head waits for its plane fences before the flip).
 *            A GPU-copy frame: the pass's own fence (a sync_file from
 *            EGL_ANDROID_native_fence_sync), or a CPU signal after the pass
 *            was waited for.
 *   release  queried (SYNCOBJ_QUERY), and watched without a thread: every
 *            point waited for is registered on ONE eventfd
 *            (DRM_IOCTL_SYNCOBJ_EVENTFD), which the backend polls; a kernel
 *            without that ioctl is polled from the tick instead.
 *
 * Syncobjs are not tied to a device for the compositor: it imports the
 * timeline's fd on its own DRM file.  The render node used here is the
 * importing GPU's (the compositor's main device) when it is known.
 */
#ifndef NB_XSYNC_H
#define NB_XSYNC_H

#include <stdbool.h>
#include <stdint.h>
#include <stddef.h>
#include <sys/types.h>

struct nb_xs;

/*
 * Open on `node` (a render node path), else on the render node of `dev` (a
 * DRM device number, 0 = unknown), else the first render node with timeline
 * syncobjs.  NULL with a reason in why.
 */
struct nb_xs *nb_xs_open(const char *node, dev_t dev, char *why, size_t n);
void nb_xs_close(struct nb_xs *x);
const char *nb_xs_node(const struct nb_xs *x);

/* A new timeline: its handle, and an fd of it for the compositor (owned by
 * the caller, who closes it once it is sent).  0, or -errno. */
int  nb_xs_timeline(struct nb_xs *x, uint32_t *handle, int *fd);
void nb_xs_destroy(struct nb_xs *x, uint32_t handle);

/*
 * Put the pending writes of `dmabuf` on (handle, point).  Returns 1 when the
 * dma-buf's fence was used, 0 when the point was signalled from the CPU (the
 * export is not available), -errno on failure.
 */
int nb_xs_from_dmabuf(struct nb_xs *x, uint32_t handle, uint64_t point,
                      int dmabuf);
/* Put a sync_file's fence on (handle, point).  Does not close sync_fd. */
int nb_xs_from_syncfile(struct nb_xs *x, uint32_t handle, uint64_t point,
                        int sync_fd);
/* Signal (handle, point) now. */
int nb_xs_signal(struct nb_xs *x, uint32_t handle, uint64_t point);

/* Block until (handle, point) is signalled, at most timeout_us: the one
 * place a CPU wait is still needed (a commit that lost explicit sync).
 * 0, -ETIME, or -errno. */
int nb_xs_wait(struct nb_xs *x, uint32_t handle, uint64_t point,
               unsigned timeout_us);

/* The last signalled point of each of `n` timelines.  0, or -errno. */
int nb_xs_query(struct nb_xs *x, const uint32_t *handles, uint64_t *points,
                unsigned n);

/*
 * The eventfd that fires when a watched point is signalled, or -1 when the
 * kernel cannot (then poll nb_xs_query from a timer).  nb_xs_watch registers
 * one (handle, point); nb_xs_drain empties the eventfd after it fired.
 */
int  nb_xs_eventfd(const struct nb_xs *x);
int  nb_xs_watch(struct nb_xs *x, uint32_t handle, uint64_t point);
void nb_xs_drain(struct nb_xs *x);

#endif /* NB_XSYNC_H */
