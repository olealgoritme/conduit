/* SPDX-License-Identifier: GPL-2.0 */
/*
 * nvgpu_compat.h -- what the module needs from kernels older than the one it
 * is written against (7.2), back to the 6.4 that packaging/dkms/dkms.conf
 * declares as the minimum.
 *
 * The rule: the module's code is written for the newest API and this header
 * maps it onto older kernels, so on a new kernel every macro here expands to
 * exactly what the code said before. Each block names the release that
 * changed the API; delete a block once that release is the minimum.
 *
 * Included from virtio_gpu_nv.c after its kernel and DRM headers.
 */
#ifndef NVGPU_COMPAT_H
#define NVGPU_COMPAT_H

#include <linux/hrtimer.h>
#include <linux/ktime.h>
#include <linux/spinlock.h>
#include <linux/version.h>
#include <linux/virtio_config.h>

#include <drm/drm_crtc.h>
#include <drm/drm_device.h>
#include <drm/drm_drv.h>
#include <drm/drm_fourcc.h>
#include <drm/drm_framebuffer.h>
#include <drm/drm_modeset_helper.h>
#include <drm/drm_print.h>
#include <drm/drm_vblank.h>

#define NVGPU_KERNEL_AT_LEAST(a, b) \
  (LINUX_VERSION_CODE >= KERNEL_VERSION(a, b, 0))

/*
 * 7.2: struct drm_atomic_state became struct drm_atomic_commit (the helper
 * callbacks take the new name). Our code says struct nvgpu_atomic_commit.
 * Not a #define of the kernel's own name: drm_atomic_commit() is also a
 * function on every kernel.
 */
#if NVGPU_KERNEL_AT_LEAST(7, 2)
#define nvgpu_atomic_commit drm_atomic_commit
#else
#define nvgpu_atomic_commit drm_atomic_state
#endif

/*
 * 6.17: drm_mode_config_funcs.fb_create and drm_helper_mode_fill_fb_struct()
 * take the format info the core already looked up. Before, the driver looks
 * it up itself (the core validated the format, so it exists).
 */
#if NVGPU_KERNEL_AT_LEAST(6, 17)
#define NVGPU_HAVE_FB_CREATE_INFO 1
#define nvgpu_fill_fb_struct(drm, fb, info, cmd) \
  drm_helper_mode_fill_fb_struct(drm, fb, info, cmd)
#else
#define nvgpu_fill_fb_struct(drm, fb, info, cmd) \
  drm_helper_mode_fill_fb_struct(drm, fb, cmd)
#endif

/*
 * 6.16: drm_plane_funcs.format_mod_supported_async, which publishes
 * IN_FORMATS_ASYNC. Before, there is no such property and the core takes any
 * format for an async flip, so leaving it out changes nothing a client sees.
 */
#if NVGPU_KERNEL_AT_LEAST(6, 16)
#define NVGPU_PLANE_FORMAT_MOD_ASYNC(fn) .format_mod_supported_async = (fn),
#else
#define NVGPU_PLANE_FORMAT_MOD_ASYNC(fn)
#endif

/*
 * 6.14: struct drm_driver lost .date. Before, DRM_IOCTL_VERSION WARNs on a
 * driver without one, so give it one there.
 */
#if NVGPU_KERNEL_AT_LEAST(6, 14)
#define NVGPU_DRM_DRIVER_DATE
#else
#define NVGPU_DRM_DRIVER_DATE .date = "20260101",
#endif

/*
 * 6.8: DRIVER_CURSOR_HOTSPOT and drm_plane_state.hotspot_x/y. The cursor
 * plane exists to hand the host the pointer image *with its hotspot*; without
 * the hotspot properties it cannot, so before 6.8 there is no cursor plane at
 * all and the guest compositor draws the cursor into the primary plane (what
 * a device without NVGPU_CFG_CURSOR gets on every kernel).
 */
#if NVGPU_KERNEL_AT_LEAST(6, 8)
#define NVGPU_HAVE_CURSOR_HOTSPOT 1
#define NVGPU_DRIVER_CURSOR_HOTSPOT DRIVER_CURSOR_HOTSPOT
#else
#define NVGPU_HAVE_CURSOR_HOTSPOT 0
#define NVGPU_DRIVER_CURSOR_HOTSPOT 0
#endif

/*
 * 6.11: virtio_find_vqs() takes an array of struct virtqueue_info instead of
 * parallel callback and name arrays. Before, provide the struct and split it.
 */
#if !NVGPU_KERNEL_AT_LEAST(6, 11)
struct virtqueue_info {
  const char *name;
  vq_callback_t *callback;
  bool ctx;
};

#define NVGPU_COMPAT_MAX_VQS 8

static inline int nvgpu_compat_find_vqs(struct virtio_device *vdev,
                                        unsigned int nvqs,
                                        struct virtqueue *vqs[],
                                        struct virtqueue_info vqs_info[],
                                        struct irq_affinity *desc) {
  vq_callback_t *callbacks[NVGPU_COMPAT_MAX_VQS];
  const char *names[NVGPU_COMPAT_MAX_VQS];
  unsigned int i;

  if (nvqs > NVGPU_COMPAT_MAX_VQS)
    return -EINVAL;
  for (i = 0; i < nvqs; i++) {
    callbacks[i] = vqs_info[i].callback;
    names[i] = vqs_info[i].name;
  }
  return virtio_find_vqs(vdev, nvqs, vqs, callbacks, names, desc);
}
#define virtio_find_vqs nvgpu_compat_find_vqs
#endif

/*
 * 6.19: <drm/drm_vblank_helper.h>, the core's hrtimer vblank
 * (DRM_CRTC_VBLANK_TIMER_FUNCS, drm_crtc_vblank_atomic_enable). Before, the
 * same thing here: an hrtimer at the mode's frame duration calling
 * drm_crtc_handle_vblank(), started and stopped by enable/disable_vblank,
 * with the vblank turned on/off in the CRTC's atomic_enable/disable -- what
 * vkms did before the core grew it, with the core's cancel-without-waiting
 * (hrtimer_cancel() from disable_vblank can deadlock on the vblank lock the
 * timer function takes).
 */
#if __has_include(<drm/drm_vblank_helper.h>)
#include <drm/drm_vblank_helper.h>
#define NVGPU_HAVE_VBLANK_TIMER 1
#else
struct nvgpu_vblank_timer {
  struct hrtimer timer;
  struct drm_crtc *crtc; /* set on first start */
  spinlock_t interval_lock;
  ktime_t interval; /* 0: cancelled, the timer function stops itself */
};

static inline struct drm_vblank_crtc *nvgpu_vblank_crtc(struct drm_crtc *crtc) {
  return &crtc->dev->vblank[drm_crtc_index(crtc)];
}

static enum hrtimer_restart nvgpu_vblank_timer_fn(struct hrtimer *timer) {
  struct nvgpu_vblank_timer *vt =
      container_of(timer, struct nvgpu_vblank_timer, timer);
  unsigned long flags;
  ktime_t interval;

  spin_lock_irqsave(&vt->interval_lock, flags);
  interval = vt->interval;
  spin_unlock_irqrestore(&vt->interval_lock, flags);
  if (!interval)
    return HRTIMER_NORESTART;

  /* Forward first, as hardware latches before it interrupts; the timestamp
   * below subtracts the frame again. */
  if (hrtimer_forward_now(timer, interval) != 1)
    drm_dbg_vbl(vt->crtc->dev, "vblank timer overrun\n");
  if (!drm_crtc_handle_vblank(vt->crtc))
    return HRTIMER_NORESTART;
  return HRTIMER_RESTART;
}

static inline int nvgpu_vblank_timer_start(struct nvgpu_vblank_timer *vt,
                                           struct drm_crtc *crtc) {
  struct drm_vblank_crtc *vblank = nvgpu_vblank_crtc(crtc);
  unsigned long flags;

  if (!vt->crtc) {
    vt->crtc = crtc;
    spin_lock_init(&vt->interval_lock);
#if NVGPU_KERNEL_AT_LEAST(6, 13)
    hrtimer_setup(&vt->timer, nvgpu_vblank_timer_fn, CLOCK_MONOTONIC,
                  HRTIMER_MODE_REL);
#else
    hrtimer_init(&vt->timer, CLOCK_MONOTONIC, HRTIMER_MODE_REL);
    vt->timer.function = nvgpu_vblank_timer_fn;
#endif
  } else {
    /* A previous cancel may not have finished yet. */
    while (hrtimer_active(&vt->timer))
      hrtimer_try_to_cancel(&vt->timer);
  }

  drm_calc_timestamping_constants(crtc, &crtc->mode);

  spin_lock_irqsave(&vt->interval_lock, flags);
  vt->interval = ns_to_ktime(vblank->framedur_ns);
  spin_unlock_irqrestore(&vt->interval_lock, flags);

  hrtimer_start(&vt->timer, vt->interval, HRTIMER_MODE_REL);
  return 0;
}

/* From disable_vblank, under the vblank lock: never waits. */
static inline void nvgpu_vblank_timer_cancel(struct nvgpu_vblank_timer *vt) {
  unsigned long flags;

  if (!vt->crtc)
    return;
  spin_lock_irqsave(&vt->interval_lock, flags);
  vt->interval = 0;
  spin_unlock_irqrestore(&vt->interval_lock, flags);
  hrtimer_try_to_cancel(&vt->timer);
}

/* Teardown, in process context with vblank already off: waits. */
static inline void nvgpu_vblank_timer_fini(struct nvgpu_vblank_timer *vt) {
  if (vt->crtc)
    hrtimer_cancel(&vt->timer);
}

/* The vblank that the running timer stands for: its expiry less a frame. */
static inline void nvgpu_vblank_timer_timestamp(struct nvgpu_vblank_timer *vt,
                                                struct drm_crtc *crtc,
                                                ktime_t *vblank_time) {
  struct drm_vblank_crtc *vblank = nvgpu_vblank_crtc(crtc);
  ktime_t cur_time;
  u64 cur_count;

  if (!vt->crtc || !READ_ONCE(vblank->enabled)) {
    *vblank_time = ktime_get();
    return;
  }
  /* A concurrent expiry can move the timer under us: reread until both
   * fields agree. */
  do {
    cur_count = drm_crtc_vblank_count_and_time(crtc, &cur_time);
    *vblank_time = READ_ONCE(vt->timer.node.expires);
  } while (cur_count != drm_crtc_vblank_count_and_time(crtc, &cur_time));

  if (drm_WARN_ON(crtc->dev, !ktime_compare(*vblank_time, cur_time)))
    return;
  *vblank_time = ktime_sub(*vblank_time, vt->interval);
}
#endif /* drm_vblank_helper.h */

#endif /* NVGPU_COMPAT_H */
