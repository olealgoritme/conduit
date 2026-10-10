/* SPDX-License-Identifier: GPL-2.0 */
/*
 * nvgpu_kms.h -- the guest half of zero-copy scanout (docs/SCANOUT.md).
 *
 * Included once, from conduit_gpu.c, after the GEM proxy and the control
 * queue: it uses both directly and is not a separate translation unit, so the
 * module stays one object for the in-tree (libkrunfw) build.
 *
 * What is here:
 *
 *   - A virtual KMS head on the first DRM device: one CRTC, one primary plane,
 *     one virtual encoder and connector, mode from config space. Only when the
 *     device announces NVGPU_CFG_DISPLAY; without it the node is render-only
 *     exactly as before.
 *
 *   - Framebuffers wrap the GEM proxies (struct nvgpu_gem_object). Nothing in
 *     the guest touches the pixels: they are host VRAM. A commit that puts a
 *     new framebuffer on the plane sends ScanoutFlip{owner_handle,
 *     host_handle, geometry, fourcc, modifier, seq} on the control queue
 *     *fire-and-forget* -- the commit never waits for the host, and the flip
 *     goes out the moment the commit lands, not at the next vblank.
 *
 *   - Vblank from the core's hrtimer (DRM_CRTC_VBLANK_TIMER_FUNCS; before
 *     6.19 the same timer from nvgpu_compat.h) at the mode's refresh.
 *     Synchronous flips complete on it; async flips
 *     (DRM_MODE_PAGE_FLIP_ASYNC, the "tearing" path) complete at once.
 *
 *   - Input: a keyboard, a relative mouse and an absolute pointer, fed from
 *     InputEvent batches on the event queue, injected in the interrupt that
 *     delivers them.
 *
 *   - A cursor plane, when the device announces NVGPU_CFG_CURSOR: its
 *     framebuffer (ARGB8888 LINEAR, up to 256x256) and hotspot go to the host
 *     as CursorUpdate, again fire-and-forget, and the viewer makes it the HOST
 *     pointer's image -- so the cursor moves at host speed and a move never
 *     crosses the boundary at all (plane position changes send nothing).
 *     Without the flag there is no cursor plane and compositors draw the
 *     cursor into the primary plane, as before.
 *
 *   - Dynamic modes: a DisplayMode event (host -> guest, event queue) makes
 *     its mode the connector's preferred one and fires a hotplug event. The
 *     connector carries "hotplug_mode_update" and suggested X/Y, the way
 *     qxl/vmwgfx do, so mutter and KWin apply the preferred mode on hotplug
 *     by themselves. wlroots compositors and Hyprland re-read modes only on
 *     disconnect -> connect, so if the head still shows another mode after a
 *     grace period, the connector is replugged once (nvgpu_replug.h) and a
 *     "preferred" output rule applies the new mode there too.
 *     The boot mode from config space stays in the list.
 *
 * No dumb buffers either:
 * there is no guest-side memory to back one, and NVIDIA's GBM allocates
 * scanout buffers through the forwarded nvidia-drm GEM ioctls. That also
 * means no fbcon on this head (CONFIG_DRM_FBDEV_EMULATION is off anyway).
 */

#include <drm/drm_atomic.h>
#include <drm/drm_atomic_helper.h>
#include <drm/drm_connector.h>
#include <drm/drm_crtc.h>
#include <drm/drm_edid.h>
#include <drm/drm_encoder.h>
#include <drm/drm_fourcc.h>
#include <drm/drm_framebuffer.h>
#include <drm/drm_gem_framebuffer_helper.h>
#include <drm/drm_managed.h>
#include <drm/drm_mode_config.h>
#include <drm/drm_modes.h>
#include <drm/drm_modeset_helper.h>
#include <drm/drm_modeset_helper_vtables.h>
#include <drm/drm_modeset_lock.h>
#include <drm/drm_plane.h>
#include <drm/drm_probe_helper.h>
#include <drm/drm_vblank.h>
#include <linux/input.h>
#include <linux/rcupdate.h>

#include "nvgpu_modelist.h"
#include "nvgpu_replug.h"

/* ───────── wire structs (docs/SCANOUT.md) ───────── */

struct nvgpu_scanout_flip_msg {
  struct nvgpu_msg_hdr hdr;
  __le32 scanout;      /* 0; one scanout for now */
  __le32 owner_handle; /* nvgpu_gem_object.owner_handle */
  __le32 host_handle;  /* nvgpu_gem_object.host_handle */
  __le32 width;
  __le32 height;
  __le32 stride; /* plane 0 pitch, bytes */
  __le32 offset; /* plane 0 offset, bytes */
  __le32 fourcc; /* DRM_FORMAT_* */
  __le64 modifier;
  __le64 seq; /* monotonically increasing per flip */
  __le32 reserved[4];
} __packed;

struct nvgpu_scanout_disable_msg {
  struct nvgpu_msg_hdr hdr;
  __le32 scanout;
  __le32 pad;
} __packed;

#define NVGPU_CURSOR_F_VISIBLE (1u << 0)
#define NVGPU_CURSOR_MAX 256

struct nvgpu_cursor_update_msg {
  struct nvgpu_msg_hdr hdr;
  __le32 scanout;
  __le32 width;
  __le32 height;
  __le32 hot_x;
  __le32 hot_y;
  __le32 owner_handle;
  __le32 host_handle;
  __le32 stride;
  __le32 offset;
  __le32 fourcc;
  __le64 modifier;
  __le32 crtc_x; /* informational: the host pointer places the cursor */
  __le32 crtc_y;
  __le32 flags; /* NVGPU_CURSOR_F_* */
  __le32 seq;
} __packed;

/* Event-queue payload of NVGPU_MSG_DISPLAY_MODE. */
struct nvgpu_display_mode_ev {
  __le32 scanout;
  __le32 width;
  __le32 height;
  __le32 refresh_mhz; /* 0: keep the current rate */
} __packed;

static_assert(sizeof(struct nvgpu_scanout_flip_msg) ==
                  sizeof(struct nvgpu_msg_hdr) + 64,
              "scanout_flip must be 64 bytes after the header");
static_assert(sizeof(struct nvgpu_scanout_disable_msg) ==
                  sizeof(struct nvgpu_msg_hdr) + 8,
              "scanout_disable must be 8 bytes after the header");
static_assert(sizeof(struct nvgpu_cursor_update_msg) ==
                  sizeof(struct nvgpu_msg_hdr) + 64,
              "cursor_update must be 64 bytes after the header");
static_assert(sizeof(struct nvgpu_display_mode_ev) == 16,
              "display_mode must be 16 bytes");

/* ───────── the head ───────── */

#define NVGPU_KMS_DEFAULT_W 2560
#define NVGPU_KMS_DEFAULT_H 1440
#define NVGPU_KMS_DEFAULT_HZ 240
#define NVGPU_KMS_MIN_DIM 64
#define NVGPU_KMS_MAX_DIM 16384
#define NVGPU_KMS_MAX_HZ 1000

/* A replug's disconnect lasts until the master probed, plus this, or at most
 * NVGPU_REPLUG_OFF_MS when nobody probes. */
#define NVGPU_REPLUG_KICK_MS 100
#define NVGPU_REPLUG_OFF_MS 2000

/* Six block-linear GOB heights, LINEAR, and the terminator. */
#define NVGPU_KMS_MAX_MODIFIERS 8

struct nvgpu_kms {
  struct drm_device *drm;
  struct nvgpu_device *dev;
  struct nvgpu_dri_dev *dri;

  struct drm_plane primary;
  struct drm_plane cursor; /* only with has_cursor */
  struct drm_crtc crtc;
  struct drm_encoder encoder;
  struct drm_connector connector;

  /* The preferred mode, which DisplayMode events change; read by get_modes
   * and written by the mode work, both under mode_config.mutex. */
  u32 width, height, refresh_mhz;
  /* The mode config space announced, kept in the list whatever happens. */
  u32 boot_w, boot_h, boot_mhz;
  /* The host's mode list (nvgpu_modelist.h), once one arrived: then the
   * connector offers exactly it. Under mode_config.mutex. */
  bool has_list;
  struct nvgpu_modelist list;
  u64 modifiers[NVGPU_KMS_MAX_MODIFIERS];
  bool has_cursor;

  /* A DisplayMode event, from the event-queue interrupt to the work that
   * applies it. Only the newest matters. */
  spinlock_t mode_lock;
  bool mode_pending;
  u32 pend_w, pend_h, pend_mhz;
  /* A DisplayModeList, likewise; only the newest matters. */
  bool list_pending;
  struct nvgpu_modelist pend_list;
  struct nvgpu_modelist work_list; /* the mode work's own copy */
  struct work_struct mode_work;

  /* The replug for compositors that ignore a connected -> connected hotplug
   * (nvgpu_replug.h): its state under mode_config.mutex, and its timer. */
  struct nvgpu_replug replug;
  struct delayed_work replug_work;

  /* What the host was last told about the cursor, so a commit that only
   * moved it sends nothing. Commit tails only, ordered per plane. */
  bool cur_sent, cur_vis;
  u32 cur_owner, cur_handle, cur_w, cur_h, cur_pitch, cur_hx, cur_hy;
  u32 cur_seq;
  u64 n_cursor;

  atomic64_t seq;
  /* Whether the host was last told a framebuffer (vs. disabled). Written
   * only from commit tails, which the atomic helpers order per CRTC. */
  bool scanout_on;
  /* Set by nvgpu_display_quiesce(): the queue is going away, send nothing. */
  bool dead;

  u64 n_flips;
  u64 n_flip_errors;

#ifndef NVGPU_HAVE_VBLANK_TIMER
  /* The vblank hrtimer the core keeps itself from 6.19 (nvgpu_compat.h). */
  struct nvgpu_vblank_timer vblank;
#endif
};

static const u32 nvgpu_kms_cursor_formats[] = {DRM_FORMAT_ARGB8888};
static const u64 nvgpu_kms_cursor_modifiers[] = {DRM_FORMAT_MOD_LINEAR,
                                                 DRM_FORMAT_MOD_INVALID};

static const u32 nvgpu_kms_formats[] = {
    DRM_FORMAT_XRGB8888,    DRM_FORMAT_ARGB8888,    DRM_FORMAT_XBGR8888,
    DRM_FORMAT_ABGR8888,    DRM_FORMAT_XRGB2101010, DRM_FORMAT_ARGB2101010,
    DRM_FORMAT_XBGR2101010, DRM_FORMAT_ABGR2101010,
};

/* ───────── messages to the backend ───────── */

static void nvgpu_kms_send_flip(struct nvgpu_kms *kms,
                                struct drm_framebuffer *fb) {
  struct nvgpu_gem_object *ng;
  struct nvgpu_scanout_flip_msg m = {};
  int ret;

  if (READ_ONCE(kms->dead) || !fb->obj[0] ||
      fb->obj[0]->funcs != &nvgpu_gem_funcs)
    return;
  ng = to_nvgpu_gem(fb->obj[0]);

  m.hdr.msg_type = cpu_to_le32(NVGPU_MSG_SCANOUT_FLIP);
  m.hdr.handle = cpu_to_le32(ng->owner_handle);
  m.scanout = 0;
  m.owner_handle = cpu_to_le32(ng->owner_handle);
  m.host_handle = cpu_to_le32(ng->host_handle);
  m.width = cpu_to_le32(fb->width);
  m.height = cpu_to_le32(fb->height);
  m.stride = cpu_to_le32(fb->pitches[0]);
  m.offset = cpu_to_le32(fb->offsets[0]);
  m.fourcc = cpu_to_le32(fb->format->format);
  m.modifier = cpu_to_le64(fb->modifier);
  m.seq = cpu_to_le64((u64)atomic64_inc_return(&kms->seq));

  /* GFP_NOWAIT: the commit tail is a dma-fence signalling section, where
   * reclaim is forbidden -- and a flip is not worth waiting for memory. */
  ret = nvgpu_send_async(kms->dev, &m, sizeof(m), sizeof(struct nvgpu_msg_hdr),
                         GFP_NOWAIT | __GFP_NOWARN);
  if (ret) {
    kms->n_flip_errors++;
    dev_warn_ratelimited(&kms->dev->vdev->dev,
                         "conduit-gpu: scanout flip dropped: %d\n", ret);
    return;
  }
  if (!kms->n_flips++)
    dev_info(&kms->dev->vdev->dev,
             "conduit-gpu: first scanout flip %ux%u fmt=%p4cc mod=%#llx "
             "pitch=%u owner=%u handle=%u\n",
             fb->width, fb->height, &fb->format->format, fb->modifier,
             fb->pitches[0], ng->owner_handle, ng->host_handle);
  WRITE_ONCE(kms->scanout_on, true);
}

static void nvgpu_kms_send_disable(struct nvgpu_kms *kms) {
  struct nvgpu_scanout_disable_msg m = {};

  if (!READ_ONCE(kms->scanout_on))
    return;
  WRITE_ONCE(kms->scanout_on, false);
  if (READ_ONCE(kms->dead))
    return;

  m.hdr.msg_type = cpu_to_le32(NVGPU_MSG_SCANOUT_DISABLE);
  m.scanout = 0;
  if (nvgpu_send_async(kms->dev, &m, sizeof(m), sizeof(struct nvgpu_msg_hdr),
                       GFP_NOWAIT | __GFP_NOWARN))
    dev_warn_ratelimited(&kms->dev->vdev->dev,
                         "conduit-gpu: scanout disable dropped\n");
}

/* ───────── framebuffers: GEM proxies, untouched ───────── */

static const struct drm_framebuffer_funcs nvgpu_kms_fb_funcs = {
    .destroy = drm_gem_fb_destroy,
    .create_handle = drm_gem_fb_create_handle,
};

static struct drm_framebuffer *
nvgpu_kms_fb_create(struct drm_device *drm, struct drm_file *file,
                    const struct drm_format_info *info,
                    const struct drm_mode_fb_cmd2 *cmd) {
  struct drm_gem_object *obj;
  struct drm_framebuffer *fb;
  int ret;

  /* Every format this head takes is single-plane. */
  if (info->num_planes != 1)
    return ERR_PTR(-EINVAL);

  obj = drm_gem_object_lookup(file, cmd->handles[0]);
  if (!obj)
    return ERR_PTR(-ENOENT);
  /* Only a proxy names host memory; anything else has nothing to scan out. */
  if (obj->funcs != &nvgpu_gem_funcs) {
    drm_gem_object_put(obj);
    return ERR_PTR(-EINVAL);
  }

  /*
   * Bounds-check a pitch-linear buffer against the object. A block-linear
   * one is laid out in GOBs whose footprint the pitch does not describe, and
   * the host driver validates it on import; nvidia-drm does not check it here
   * either.
   */
  if (!(cmd->flags & DRM_MODE_FB_MODIFIERS) ||
      cmd->modifier[0] == DRM_FORMAT_MOD_LINEAR) {
    u64 min_size = (u64)(cmd->height - 1) * cmd->pitches[0] +
                   (u64)cmd->width * info->cpp[0] + cmd->offsets[0];

    if (cmd->height == 0 || min_size > obj->size) {
      drm_gem_object_put(obj);
      return ERR_PTR(-EINVAL);
    }
  }

  fb = kzalloc(sizeof(*fb), GFP_KERNEL);
  if (!fb) {
    drm_gem_object_put(obj);
    return ERR_PTR(-ENOMEM);
  }
  nvgpu_fill_fb_struct(drm, fb, info, cmd);
  fb->obj[0] = obj;

  ret = drm_framebuffer_init(drm, fb, &nvgpu_kms_fb_funcs);
  if (ret) {
    drm_gem_object_put(obj);
    kfree(fb);
    return ERR_PTR(ret);
  }
  return fb;
}

#ifndef NVGPU_HAVE_FB_CREATE_INFO
/* Before 6.17 the core does not pass the format info (nvgpu_compat.h). */
static struct drm_framebuffer *
nvgpu_kms_fb_create_noinfo(struct drm_device *drm, struct drm_file *file,
                           const struct drm_mode_fb_cmd2 *cmd) {
  const struct drm_format_info *info = drm_get_format_info(drm, cmd);

  if (!info)
    return ERR_PTR(-EINVAL);
  return nvgpu_kms_fb_create(drm, file, info, cmd);
}
#define nvgpu_kms_fb_create nvgpu_kms_fb_create_noinfo
#endif

/* ───────── primary plane ───────── */

static bool nvgpu_kms_format_mod_supported(struct drm_plane *plane, u32 format,
                                           u64 modifier) {
  /*
   * LINEAR, and any NVIDIA modifier: the buffer is the host GPU's own and the
   * host imports it with the modifier it was allocated with, so the guest
   * has no reason to second-guess a kind or GOB height. What is *advertised*
   * (IN_FORMATS) is the narrower list nvidia-drm itself would offer.
   */
  if (modifier == DRM_FORMAT_MOD_LINEAR)
    return true;
  if (modifier == DRM_FORMAT_MOD_INVALID)
    return false;
  return fourcc_mod_is_vendor(modifier, NVIDIA);
}

static int nvgpu_kms_plane_atomic_check(struct drm_plane *plane,
                                        struct nvgpu_atomic_commit *state) {
  struct drm_plane_state *new = drm_atomic_get_new_plane_state(state, plane);
  struct drm_crtc_state *crtc_state;

  if (!new->crtc || !new->fb)
    return 0;
  crtc_state = drm_atomic_get_new_crtc_state(state, new->crtc);
  if (!crtc_state)
    return 0;
  return drm_atomic_helper_check_plane_state(new, crtc_state,
                                             DRM_PLANE_NO_SCALING,
                                             DRM_PLANE_NO_SCALING, false, true);
}

/*
 * Runs in the commit tail, in plane order before the CRTC's flush: the host
 * hears about the frame here, before the flip event is armed, so a host
 * present never waits on the guest's vblank.
 */
static void nvgpu_kms_plane_atomic_update(struct drm_plane *plane,
                                          struct nvgpu_atomic_commit *state) {
  struct nvgpu_kms *kms = container_of(plane, struct nvgpu_kms, primary);
  struct drm_plane_state *new = drm_atomic_get_new_plane_state(state, plane);

  if (new->fb && new->crtc && new->visible)
    nvgpu_kms_send_flip(kms, new->fb);
  else
    nvgpu_kms_send_disable(kms);
}

static const struct drm_plane_helper_funcs nvgpu_kms_plane_helper_funcs = {
    .atomic_check = nvgpu_kms_plane_atomic_check,
    .atomic_update = nvgpu_kms_plane_atomic_update,
    /* prepare_fb: the core's GEM default (implicit fences on the proxy). */
};

static const struct drm_plane_funcs nvgpu_kms_plane_funcs = {
    .update_plane = drm_atomic_helper_update_plane,
    .disable_plane = drm_atomic_helper_disable_plane,
    .destroy = drm_plane_cleanup,
    .reset = drm_atomic_helper_plane_reset,
    .atomic_duplicate_state = drm_atomic_helper_plane_duplicate_state,
    .atomic_destroy_state = drm_atomic_helper_plane_destroy_state,
    .format_mod_supported = nvgpu_kms_format_mod_supported,
    /* Async flips take the same buffers; this also publishes
     * IN_FORMATS_ASYNC so a compositor knows tearing works with them. */
    NVGPU_PLANE_FORMAT_MOD_ASYNC(nvgpu_kms_format_mod_supported)
};

/* ───────── cursor plane ───────── */

/*
 * Tell the host the cursor's image, hotspot or visibility changed. Never a
 * move: the host pointer places the cursor itself, which is the point.
 * GFP_NOWAIT for the same reason as a flip: this is the commit tail.
 */
static void nvgpu_kms_send_cursor(struct nvgpu_kms *kms,
                                  struct drm_plane_state *st) {
  struct nvgpu_cursor_update_msg m = {};
  struct drm_framebuffer *fb = st->fb;
  bool vis = fb && st->crtc && st->visible && fb->obj[0] &&
             fb->obj[0]->funcs == &nvgpu_gem_funcs;
  u32 owner = 0, handle = 0, w = 0, h = 0, pitch = 0, hx = 0, hy = 0;
  int ret;

  if (READ_ONCE(kms->dead))
    return;
  if (vis) {
    struct nvgpu_gem_object *ng = to_nvgpu_gem(fb->obj[0]);

    owner = ng->owner_handle;
    handle = ng->host_handle;
    w = min_t(u32, fb->width, NVGPU_CURSOR_MAX);
    h = min_t(u32, fb->height, NVGPU_CURSOR_MAX);
    pitch = fb->pitches[0];
#if NVGPU_HAVE_CURSOR_HOTSPOT
    hx = clamp_t(s32, st->hotspot_x, 0, (s32)w - 1);
    hy = clamp_t(s32, st->hotspot_y, 0, (s32)h - 1);
#endif
  }
  if (kms->cur_sent && vis == kms->cur_vis &&
      (!vis || (owner == kms->cur_owner && handle == kms->cur_handle &&
                w == kms->cur_w && h == kms->cur_h && pitch == kms->cur_pitch &&
                hx == kms->cur_hx && hy == kms->cur_hy)))
    return;

  m.hdr.msg_type = cpu_to_le32(NVGPU_MSG_CURSOR_UPDATE);
  m.hdr.handle = cpu_to_le32(owner);
  m.scanout = 0;
  if (vis) {
    m.width = cpu_to_le32(w);
    m.height = cpu_to_le32(h);
    m.hot_x = cpu_to_le32(hx);
    m.hot_y = cpu_to_le32(hy);
    m.owner_handle = cpu_to_le32(owner);
    m.host_handle = cpu_to_le32(handle);
    m.stride = cpu_to_le32(pitch);
    m.offset = cpu_to_le32(fb->offsets[0]);
    m.fourcc = cpu_to_le32(fb->format->format);
    m.modifier = cpu_to_le64(fb->modifier);
    m.crtc_x = cpu_to_le32((u32)st->crtc_x);
    m.crtc_y = cpu_to_le32((u32)st->crtc_y);
    m.flags = cpu_to_le32(NVGPU_CURSOR_F_VISIBLE);
  }
  m.seq = cpu_to_le32(++kms->cur_seq);
  ret = nvgpu_send_async(kms->dev, &m, sizeof(m), sizeof(struct nvgpu_msg_hdr),
                         GFP_NOWAIT | __GFP_NOWARN);
  if (ret) {
    /* Not remembered as sent: the next cursor commit tries again. */
    dev_warn_ratelimited(&kms->dev->vdev->dev,
                         "conduit-gpu: cursor update dropped: %d\n", ret);
    kms->cur_sent = false;
    return;
  }
  if (!kms->n_cursor++)
    dev_info(&kms->dev->vdev->dev,
             "conduit-gpu: first cursor %ux%u hot %u,%u -> host pointer\n",
             w, h, hx, hy);
  kms->cur_sent = true;
  kms->cur_vis = vis;
  kms->cur_owner = owner;
  kms->cur_handle = handle;
  kms->cur_w = w;
  kms->cur_h = h;
  kms->cur_pitch = pitch;
  kms->cur_hx = hx;
  kms->cur_hy = hy;
}

static int nvgpu_kms_cursor_atomic_check(struct drm_plane *plane,
                                         struct nvgpu_atomic_commit *state) {
  struct drm_plane_state *new = drm_atomic_get_new_plane_state(state, plane);
  struct drm_crtc_state *crtc_state;

  if (!new->crtc || !new->fb)
    return 0;
  if (new->fb->width > NVGPU_CURSOR_MAX || new->fb->height > NVGPU_CURSOR_MAX)
    return -EINVAL;
  crtc_state = drm_atomic_get_new_crtc_state(state, new->crtc);
  if (!crtc_state)
    return 0;
  /* Positionable and allowed off the edge: it is a cursor. */
  return drm_atomic_helper_check_plane_state(new, crtc_state,
                                             DRM_PLANE_NO_SCALING,
                                             DRM_PLANE_NO_SCALING, true, true);
}

static void nvgpu_kms_cursor_atomic_update(struct drm_plane *plane,
                                           struct nvgpu_atomic_commit *state) {
  struct nvgpu_kms *kms = container_of(plane, struct nvgpu_kms, cursor);

  nvgpu_kms_send_cursor(kms, drm_atomic_get_new_plane_state(state, plane));
}

static const struct drm_plane_helper_funcs nvgpu_kms_cursor_helper_funcs = {
    .atomic_check = nvgpu_kms_cursor_atomic_check,
    .atomic_update = nvgpu_kms_cursor_atomic_update,
};

static bool nvgpu_kms_cursor_format_mod_supported(struct drm_plane *plane,
                                                  u32 format, u64 modifier) {
  return format == DRM_FORMAT_ARGB8888 && modifier == DRM_FORMAT_MOD_LINEAR;
}

static const struct drm_plane_funcs nvgpu_kms_cursor_funcs = {
    .update_plane = drm_atomic_helper_update_plane,
    .disable_plane = drm_atomic_helper_disable_plane,
    .destroy = drm_plane_cleanup,
    .reset = drm_atomic_helper_plane_reset,
    .atomic_duplicate_state = drm_atomic_helper_plane_duplicate_state,
    .atomic_destroy_state = drm_atomic_helper_plane_destroy_state,
    .format_mod_supported = nvgpu_kms_cursor_format_mod_supported,
};

/* ───────── CRTC ───────── */

/*
 * Deliver the flip event: on the next timer vblank for an ordinary flip, at
 * once for an async (tearing) one or when vblank is off. The host was told
 * about the buffer already, in the plane update.
 */
static void nvgpu_kms_crtc_atomic_flush(struct drm_crtc *crtc,
                                        struct nvgpu_atomic_commit *state) {
  struct drm_crtc_state *cs = drm_atomic_get_new_crtc_state(state, crtc);
  struct drm_pending_vblank_event *event = cs->event;

  if (!event)
    return;
  cs->event = NULL;

  spin_lock_irq(&crtc->dev->event_lock);
  if (cs->async_flip || drm_crtc_vblank_get(crtc) != 0)
    drm_crtc_send_vblank_event(crtc, event);
  else
    drm_crtc_arm_vblank_event(crtc, event);
  spin_unlock_irq(&crtc->dev->event_lock);
}

static void nvgpu_kms_crtc_atomic_disable(struct drm_crtc *crtc,
                                          struct nvgpu_atomic_commit *state) {
  struct nvgpu_kms *kms = container_of(crtc, struct nvgpu_kms, crtc);

  /* Sends any event still armed, so no compositor waits on a dead head. */
  drm_crtc_vblank_off(crtc);
  nvgpu_kms_send_disable(kms);
}

#ifdef NVGPU_HAVE_VBLANK_TIMER
#define NVGPU_KMS_CRTC_VBLANK_FUNCS DRM_CRTC_VBLANK_TIMER_FUNCS
#define nvgpu_kms_crtc_atomic_enable drm_crtc_vblank_atomic_enable
#else
/* Before 6.19: the same timer vblank, from nvgpu_compat.h. */
static int nvgpu_kms_enable_vblank(struct drm_crtc *crtc) {
  struct nvgpu_kms *kms = container_of(crtc, struct nvgpu_kms, crtc);

  return nvgpu_vblank_timer_start(&kms->vblank, crtc);
}

static void nvgpu_kms_disable_vblank(struct drm_crtc *crtc) {
  struct nvgpu_kms *kms = container_of(crtc, struct nvgpu_kms, crtc);

  nvgpu_vblank_timer_cancel(&kms->vblank);
}

static bool nvgpu_kms_get_vblank_timestamp(struct drm_crtc *crtc,
                                           int *max_error, ktime_t *vblank_time,
                                           bool in_vblank_irq) {
  struct nvgpu_kms *kms = container_of(crtc, struct nvgpu_kms, crtc);

  nvgpu_vblank_timer_timestamp(&kms->vblank, crtc, vblank_time);
  return true;
}

static void nvgpu_kms_crtc_atomic_enable(struct drm_crtc *crtc,
                                         struct nvgpu_atomic_commit *state) {
  drm_crtc_vblank_on(crtc);
}

#define NVGPU_KMS_CRTC_VBLANK_FUNCS                                            \
  .enable_vblank = nvgpu_kms_enable_vblank,                                    \
  .disable_vblank = nvgpu_kms_disable_vblank,                                  \
  .get_vblank_timestamp = nvgpu_kms_get_vblank_timestamp
#endif

static const struct drm_crtc_helper_funcs nvgpu_kms_crtc_helper_funcs = {
    .atomic_flush = nvgpu_kms_crtc_atomic_flush,
    .atomic_enable = nvgpu_kms_crtc_atomic_enable,
    .atomic_disable = nvgpu_kms_crtc_atomic_disable,
};

static const struct drm_crtc_funcs nvgpu_kms_crtc_funcs = {
    .reset = drm_atomic_helper_crtc_reset,
    .destroy = drm_crtc_cleanup,
    .set_config = drm_atomic_helper_set_config,
    .page_flip = drm_atomic_helper_page_flip,
    .atomic_duplicate_state = drm_atomic_helper_crtc_duplicate_state,
    .atomic_destroy_state = drm_atomic_helper_crtc_destroy_state,
    NVGPU_KMS_CRTC_VBLANK_FUNCS,
};

/* ───────── encoder and connector ───────── */

static const struct drm_encoder_funcs nvgpu_kms_encoder_funcs = {
    .destroy = drm_encoder_cleanup,
};

/*
 * The configured mode, with exact timings rather than CVT's: a virtual head
 * has no blanking to honour, and the vblank timer's period is derived from
 * these numbers, so the pixel clock is chosen to make it 1/hz.
 */
static struct drm_display_mode *nvgpu_kms_make_mode(struct drm_device *drm,
                                                    u32 w, u32 h, u32 mhz,
                                                    bool preferred) {
  struct drm_display_mode *m = drm_mode_create(drm);

  if (!m)
    return NULL;
  m->hdisplay = w;
  m->hsync_start = w + 48;
  m->hsync_end = w + 80;
  m->htotal = w + 160;
  m->vdisplay = h;
  m->vsync_start = h + 3;
  m->vsync_end = h + 8;
  m->vtotal = h + 30;
  /* kHz from millihertz: htotal * vtotal * mhz / 1e6. */
  m->clock = (int)DIV_ROUND_UP_ULL((u64)m->htotal * m->vtotal * mhz, 1000000);
  m->flags = DRM_MODE_FLAG_PHSYNC | DRM_MODE_FLAG_NVSYNC;
  m->type = DRM_MODE_TYPE_DRIVER | (preferred ? DRM_MODE_TYPE_PREFERRED : 0);
  drm_mode_set_name(m);
  return m;
}

/* With the host's list: exactly its modes, the preferred one first. */
static int nvgpu_kms_get_list_modes(struct nvgpu_kms *kms,
                                    struct drm_connector *conn) {
  u32 mhz = kms->list.refresh_mhz ? kms->list.refresh_mhz : kms->refresh_mhz;
  struct drm_display_mode *m;
  int count = 0;
  u32 i;

  m = nvgpu_kms_make_mode(conn->dev, kms->width, kms->height, kms->refresh_mhz,
                          true);
  if (m) {
    drm_mode_probed_add(conn, m);
    count++;
  }
  for (i = 0; i < kms->list.count; i++) {
    if (kms->list.w[i] == kms->width && kms->list.h[i] == kms->height &&
        mhz == kms->refresh_mhz)
      continue;
    m = nvgpu_kms_make_mode(conn->dev, kms->list.w[i], kms->list.h[i], mhz,
                            false);
    if (m) {
      drm_mode_probed_add(conn, m);
      count++;
    }
  }
  return count;
}

static int nvgpu_kms_get_modes(struct drm_connector *conn) {
  struct nvgpu_kms *kms = container_of(conn, struct nvgpu_kms, connector);
  struct drm_display_mode *m;
  int count;

  if (kms->has_list)
    return nvgpu_kms_get_list_modes(kms, conn);

  /* The standard table up to the larger of the two sizes, for a user who
   * wants less; at 60 Hz, so never mistaken for the preferred one. */
  count = drm_add_modes_noedid(conn, max(kms->width, kms->boot_w),
                               max(kms->height, kms->boot_h));

  m = nvgpu_kms_make_mode(conn->dev, kms->width, kms->height, kms->refresh_mhz,
                          true);
  if (m) {
    drm_mode_probed_add(conn, m);
    count++;
  }
  /* The boot mode stays offered after the host asked for another. */
  if (kms->boot_w != kms->width || kms->boot_h != kms->height ||
      kms->boot_mhz != kms->refresh_mhz) {
    m = nvgpu_kms_make_mode(conn->dev, kms->boot_w, kms->boot_h,
                            kms->boot_mhz, false);
    if (m) {
      drm_mode_probed_add(conn, m);
      count++;
    }
  }
  return count;
}

/* Called under mode_config.mutex, from the master's probe (GETCONNECTOR). */
static int nvgpu_kms_detect_ctx(struct drm_connector *conn,
                                struct drm_modeset_acquire_ctx *ctx,
                                bool force) {
  struct nvgpu_kms *kms = container_of(conn, struct nvgpu_kms, connector);
  bool kick;

  if (nvgpu_replug_on_probe(&kms->replug, &kick))
    return connector_status_connected;
  /* The compositor saw the disconnect: reconnect once it has dealt with it. */
  if (kick)
    mod_delayed_work(system_wq, &kms->replug_work,
                     msecs_to_jiffies(NVGPU_REPLUG_KICK_MS));
  return connector_status_disconnected;
}

static const struct drm_connector_helper_funcs nvgpu_kms_conn_helper_funcs = {
    .get_modes = nvgpu_kms_get_modes,
    .detect_ctx = nvgpu_kms_detect_ctx,
};

static const struct drm_connector_funcs nvgpu_kms_conn_funcs = {
    .fill_modes = drm_helper_probe_single_connector_modes,
    .destroy = drm_connector_cleanup,
    .reset = drm_atomic_helper_connector_reset,
    .atomic_duplicate_state = drm_atomic_helper_connector_duplicate_state,
    .atomic_destroy_state = drm_atomic_helper_connector_destroy_state,
};

/* ───────── dynamic modes ───────── */

/*
 * How long a compositor has to apply a new preferred mode after the hotplug
 * before the connector is replugged; 0 never replugs. mutter and KWin apply
 * it within a few frames.
 */
static int nvgpu_mode_replug_ms = 1000;
module_param_named(mode_replug_ms, nvgpu_mode_replug_ms, int, 0644);
MODULE_PARM_DESC(mode_replug_ms,
                 "ms to wait for a compositor to apply a host mode before "
                 "replugging the connector (0: never)");

/* Whether the head shows the preferred mode. Under mode_config.mutex. */
static bool nvgpu_kms_mode_applied(struct nvgpu_kms *kms) {
  struct drm_crtc *crtc = &kms->crtc;
  const struct drm_crtc_state *cs;
  bool on, applied;

  drm_modeset_lock(&crtc->mutex, NULL);
  cs = crtc->state;
  on = cs && cs->enable;
  applied = nvgpu_replug_applied(
      on, on ? cs->mode.hdisplay : 0, on ? cs->mode.vdisplay : 0,
      on ? drm_mode_vrefresh(&cs->mode) : 0, kms->width, kms->height,
      kms->refresh_mhz);
  drm_modeset_unlock(&crtc->mutex);
  return applied;
}

static void nvgpu_kms_replug_work(struct work_struct *work) {
  struct nvgpu_kms *kms =
      container_of(to_delayed_work(work), struct nvgpu_kms, replug_work);
  struct drm_device *drm = kms->drm;
  enum nvgpu_replug_act act;
  u32 w, h;

  if (READ_ONCE(kms->dead))
    return;
  mutex_lock(&drm->mode_config.mutex);
  act = nvgpu_replug_on_timer(&kms->replug,
                              kms->replug.stage != NVGPU_REPLUG_WATCH ||
                                  nvgpu_kms_mode_applied(kms));
  w = kms->width;
  h = kms->height;
  /* The timeout before the hotplug, so the probe's kick is not overridden. */
  if (act == NVGPU_REPLUG_DISCONNECT)
    mod_delayed_work(system_wq, &kms->replug_work,
                     msecs_to_jiffies(NVGPU_REPLUG_OFF_MS));
  mutex_unlock(&drm->mode_config.mutex);

  switch (act) {
  case NVGPU_REPLUG_DISCONNECT:
    dev_info(&kms->dev->vdev->dev,
             "conduit-gpu: the compositor kept another mode than %ux%u: "
             "replugging the connector\n",
             w, h);
    drm_kms_helper_hotplug_event(drm);
    break;
  case NVGPU_REPLUG_RECONNECT:
    drm_kms_helper_hotplug_event(drm);
    break;
  case NVGPU_REPLUG_NOTHING:
    break;
  }
}

/*
 * Apply the newest DisplayMode: it becomes the preferred mode, and a hotplug
 * event makes compositors re-probe. Process context, because the probe
 * helpers and the uevent sleep; the event-queue interrupt only records it.
 */
/*
 * Apply the newest DisplayModeList: the connector offers exactly it from now
 * on, and a changed list is a hotplug so compositors (and games' mode menus)
 * see it. The replug of nvgpu_replug.h is for a changed preferred mode only.
 */
static void nvgpu_kms_apply_list(struct nvgpu_kms *kms) {
  struct drm_device *drm = kms->drm;
  bool changed;

  spin_lock_irq(&kms->mode_lock);
  if (!kms->list_pending) {
    spin_unlock_irq(&kms->mode_lock);
    return;
  }
  kms->list_pending = false;
  kms->work_list = kms->pend_list;
  spin_unlock_irq(&kms->mode_lock);
  if (READ_ONCE(kms->dead))
    return;

  mutex_lock(&drm->mode_config.mutex);
  changed = !kms->has_list || !nvgpu_modelist_equal(&kms->list, &kms->work_list);
  kms->list = kms->work_list;
  kms->has_list = true;
  mutex_unlock(&drm->mode_config.mutex);
  if (!changed)
    return;
  dev_info(&kms->dev->vdev->dev,
           "conduit-gpu: host mode list: %u modes, %ux%u native\n",
           kms->work_list.count, kms->work_list.w[0], kms->work_list.h[0]);
  drm_kms_helper_hotplug_event(drm);
}

static void nvgpu_kms_mode_work(struct work_struct *work) {
  struct nvgpu_kms *kms = container_of(work, struct nvgpu_kms, mode_work);
  struct drm_device *drm = kms->drm;
  u32 w, h, mhz;
  bool changed;

  nvgpu_kms_apply_list(kms);
  spin_lock_irq(&kms->mode_lock);
  if (!kms->mode_pending) {
    spin_unlock_irq(&kms->mode_lock);
    return;
  }
  kms->mode_pending = false;
  w = kms->pend_w;
  h = kms->pend_h;
  mhz = kms->pend_mhz;
  spin_unlock_irq(&kms->mode_lock);
  if (READ_ONCE(kms->dead))
    return;

  mutex_lock(&drm->mode_config.mutex);
  if (!mhz)
    mhz = kms->refresh_mhz;
  changed = w != kms->width || h != kms->height || mhz != kms->refresh_mhz;
  kms->width = w;
  kms->height = h;
  kms->refresh_mhz = mhz;
  mutex_unlock(&drm->mode_config.mutex);

  if (!changed)
    return;
  dev_info(&kms->dev->vdev->dev,
           "conduit-gpu: host asks for %ux%u@%u.%03u: preferred mode "
           "changed, hotplug sent\n",
           w, h, mhz / 1000, mhz % 1000);
  drm_kms_helper_hotplug_event(drm);

  if (READ_ONCE(nvgpu_mode_replug_ms) <= 0)
    return;
  mutex_lock(&drm->mode_config.mutex);
  if (nvgpu_replug_on_mode(&kms->replug))
    mod_delayed_work(system_wq, &kms->replug_work,
                     msecs_to_jiffies(READ_ONCE(nvgpu_mode_replug_ms)));
  mutex_unlock(&drm->mode_config.mutex);
}

/* The event-queue interrupt: record, and leave the rest to the work. */
static void nvgpu_kms_mode_event(struct nvgpu_device *dev, const u8 *p,
                                 unsigned int len) {
  const struct nvgpu_display_mode_ev *ev = (const void *)p;
  struct nvgpu_kms *kms;
  unsigned long flags;
  u32 w, h, mhz;

  if (len < sizeof(*ev) || get_unaligned_le32(&ev->scanout) != 0)
    return;
  w = get_unaligned_le32(&ev->width);
  h = get_unaligned_le32(&ev->height);
  mhz = get_unaligned_le32(&ev->refresh_mhz);
  if (w < NVGPU_KMS_MIN_DIM || h < NVGPU_KMS_MIN_DIM ||
      w > NVGPU_KMS_MAX_DIM || h > NVGPU_KMS_MAX_DIM ||
      mhz > NVGPU_KMS_MAX_HZ * 1000)
    return;
  if (mhz && mhz < 1000)
    mhz = 1000;

  rcu_read_lock();
  kms = rcu_dereference(dev->kms_ev);
  if (kms && !READ_ONCE(kms->dead)) {
    spin_lock_irqsave(&kms->mode_lock, flags);
    kms->pend_w = w;
    kms->pend_h = h;
    kms->pend_mhz = mhz;
    kms->mode_pending = true;
    spin_unlock_irqrestore(&kms->mode_lock, flags);
    schedule_work(&kms->mode_work);
  }
  rcu_read_unlock();
}

/* The event-queue interrupt: a DisplayModeList. Parsed straight into the
 * pending copy; the work applies it. */
static void nvgpu_kms_mode_list_event(struct nvgpu_device *dev, const u8 *p,
                                      unsigned int len) {
  struct nvgpu_kms *kms;
  unsigned long flags;

  rcu_read_lock();
  kms = rcu_dereference(dev->kms_ev);
  if (kms && !READ_ONCE(kms->dead)) {
    spin_lock_irqsave(&kms->mode_lock, flags);
    if (nvgpu_modelist_parse(p, len, NVGPU_KMS_MIN_DIM, NVGPU_KMS_MAX_DIM,
                             NVGPU_KMS_MAX_HZ, &kms->pend_list))
      kms->list_pending = true;
    spin_unlock_irqrestore(&kms->mode_lock, flags);
    if (kms->list_pending)
      schedule_work(&kms->mode_work);
  }
  rcu_read_unlock();
}

/* ───────── mode config ───────── */

/*
 * The default commit tail, except that it waits for the flip to be done
 * rather than for a vblank. For an async flip that is immediate, so a
 * compositor flipping as fast as it renders is not paced by the commit worker
 * through cleanup_done; for an ordinary flip it is the same one vblank.
 */
static void nvgpu_kms_commit_tail(struct nvgpu_atomic_commit *state) {
  struct drm_device *drm = state->dev;

  drm_atomic_helper_commit_modeset_disables(drm, state);
  drm_atomic_helper_commit_planes(drm, state, 0);
  drm_atomic_helper_commit_modeset_enables(drm, state);
  drm_atomic_helper_fake_vblank(state);
  drm_atomic_helper_commit_hw_done(state);
  drm_atomic_helper_wait_for_flip_done(drm, state);
  drm_atomic_helper_cleanup_planes(drm, state);
}

static const struct drm_mode_config_helper_funcs nvgpu_kms_mode_helpers = {
    .atomic_commit_tail = nvgpu_kms_commit_tail,
};

static const struct drm_mode_config_funcs nvgpu_kms_mode_funcs = {
    .fb_create = nvgpu_kms_fb_create,
    .atomic_check = drm_atomic_helper_check,
    .atomic_commit = drm_atomic_helper_commit,
};

/*
 * The modifiers nvidia-drm itself advertises for this GPU: block-linear at
 * GOB heights 32..1 with the GPU's generic page kind, generation and sector
 * layout -- all from the host's GET_DEV_INFO -- then LINEAR. The NVIDIA GBM
 * and EGL in the guest pick from this list exactly as they would on bare
 * metal. Without a page kind (no video memory), LINEAR alone.
 */
static unsigned int nvgpu_kms_build_modifiers(struct nvgpu_kms *kms) {
  const struct nvgpu_devinfo *info = &kms->dri->dev_info;
  u32 kind = info->v[NVGPU_DI_GENERIC_PAGE_KIND];
  u32 gen = info->v[NVGPU_DI_PAGE_KIND_GENERATION];
  u32 sector = info->v[NVGPU_DI_SECTOR_LAYOUT];
  unsigned int n = 0;
  int h;

  if (kind) {
    for (h = 5; h >= 0; h--)
      kms->modifiers[n++] =
          DRM_FORMAT_MOD_NVIDIA_BLOCK_LINEAR_2D(0, sector, gen, kind, h);
  }
  kms->modifiers[n++] = DRM_FORMAT_MOD_LINEAR;
  kms->modifiers[n] = DRM_FORMAT_MOD_INVALID;
  return n;
}

static u32 nvgpu_kms_clamp(u32 v, u32 lo, u32 hi, u32 dflt) {
  if (!v)
    return dflt;
  return clamp(v, lo, hi);
}

/*
 * Build the head on `drm`, before drm_dev_register(). Everything is drmm-
 * managed: `kms` is allocated before the mode config so that the mode
 * config's cleanup, registered later, runs first and still finds it.
 */
static int nvgpu_kms_init(struct nvgpu_dri_dev *dri, struct drm_device *drm) {
  struct nvgpu_device *dev = dri->dev;
  struct nvgpu_kms *kms;
  int ret;

  kms = drmm_kzalloc(drm, sizeof(*kms), GFP_KERNEL);
  if (!kms)
    return -ENOMEM;
  kms->drm = drm;
  kms->dev = dev;
  kms->dri = dri;
  atomic64_set(&kms->seq, 0);
  kms->width = nvgpu_kms_clamp(dev->display_width, NVGPU_KMS_MIN_DIM,
                               NVGPU_KMS_MAX_DIM, NVGPU_KMS_DEFAULT_W);
  kms->height = nvgpu_kms_clamp(dev->display_height, NVGPU_KMS_MIN_DIM,
                                NVGPU_KMS_MAX_DIM, NVGPU_KMS_DEFAULT_H);
  kms->refresh_mhz = 1000 * nvgpu_kms_clamp(dev->display_refresh_hz, 1,
                                            NVGPU_KMS_MAX_HZ,
                                            NVGPU_KMS_DEFAULT_HZ);
  kms->boot_w = kms->width;
  kms->boot_h = kms->height;
  kms->boot_mhz = kms->refresh_mhz;
  /* No hotspot before 6.8 (nvgpu_compat.h): no cursor plane either. */
  kms->has_cursor = NVGPU_HAVE_CURSOR_HOTSPOT && dev->has_cursor;
  spin_lock_init(&kms->mode_lock);
  INIT_WORK(&kms->mode_work, nvgpu_kms_mode_work);
  INIT_DELAYED_WORK(&kms->replug_work, nvgpu_kms_replug_work);
  nvgpu_kms_build_modifiers(kms);

  ret = drmm_mode_config_init(drm);
  if (ret)
    return ret;
  drm->mode_config.min_width = 1;
  drm->mode_config.min_height = 1;
  drm->mode_config.max_width = NVGPU_KMS_MAX_DIM;
  drm->mode_config.max_height = NVGPU_KMS_MAX_DIM;
  drm->mode_config.preferred_depth = 24;
  drm->mode_config.prefer_shadow = 0;
  /* DRM_CAP_ASYNC_PAGE_FLIP and DRM_CAP_ATOMIC_ASYNC_PAGE_FLIP. */
  drm->mode_config.async_page_flip = true;
  drm->mode_config.funcs = &nvgpu_kms_mode_funcs;
  drm->mode_config.helper_private = &nvgpu_kms_mode_helpers;
  if (kms->has_cursor) {
    /* DRM_CAP_CURSOR_WIDTH/HEIGHT: what compositors size cursor bos by. */
    drm->mode_config.cursor_width = NVGPU_CURSOR_MAX;
    drm->mode_config.cursor_height = NVGPU_CURSOR_MAX;
  }

  ret = drm_vblank_init(drm, 1);
  if (ret)
    return ret;

  ret = drm_universal_plane_init(drm, &kms->primary, 0, &nvgpu_kms_plane_funcs,
                                 nvgpu_kms_formats,
                                 ARRAY_SIZE(nvgpu_kms_formats), kms->modifiers,
                                 DRM_PLANE_TYPE_PRIMARY, NULL);
  if (ret)
    return ret;
  drm_plane_helper_add(&kms->primary, &nvgpu_kms_plane_helper_funcs);

  if (kms->has_cursor) {
    ret = drm_universal_plane_init(
        drm, &kms->cursor, 0, &nvgpu_kms_cursor_funcs,
        nvgpu_kms_cursor_formats, ARRAY_SIZE(nvgpu_kms_cursor_formats),
        nvgpu_kms_cursor_modifiers, DRM_PLANE_TYPE_CURSOR, NULL);
    if (ret)
      return ret;
    drm_plane_helper_add(&kms->cursor, &nvgpu_kms_cursor_helper_funcs);
  }

  ret = drm_crtc_init_with_planes(drm, &kms->crtc, &kms->primary,
                                  kms->has_cursor ? &kms->cursor : NULL,
                                  &nvgpu_kms_crtc_funcs, NULL);
  if (ret)
    return ret;
  drm_crtc_helper_add(&kms->crtc, &nvgpu_kms_crtc_helper_funcs);

  ret = drm_encoder_init(drm, &kms->encoder, &nvgpu_kms_encoder_funcs,
                         DRM_MODE_ENCODER_VIRTUAL, NULL);
  if (ret)
    return ret;
  kms->encoder.possible_crtcs = drm_crtc_mask(&kms->crtc);

  ret = drm_connector_init(drm, &kms->connector, &nvgpu_kms_conn_funcs,
                           DRM_MODE_CONNECTOR_VIRTUAL);
  if (ret)
    return ret;
  drm_connector_helper_add(&kms->connector, &nvgpu_kms_conn_helper_funcs);
  ret = drm_connector_attach_encoder(&kms->connector, &kms->encoder);
  if (ret)
    return ret;

  /*
   * What makes mutter and KWin follow the host's window by themselves:
   * "hotplug_mode_update" says this output's preferred mode is to be applied
   * on hotplug (vmwgfx and qxl carry it for the same reason), suggested X/Y
   * where to put it.
   */
  {
    struct drm_property *hp;

    ret = drm_mode_create_suggested_offset_properties(drm);
    if (ret)
      return ret;
    hp = drm_property_create_range(drm, DRM_MODE_PROP_IMMUTABLE,
                                   "hotplug_mode_update", 0, 1);
    if (!hp)
      return -ENOMEM;
    drm_object_attach_property(&kms->connector.base, hp, 1);
    drm_object_attach_property(&kms->connector.base,
                               drm->mode_config.suggested_x_property, 0);
    drm_object_attach_property(&kms->connector.base,
                               drm->mode_config.suggested_y_property, 0);
  }

  drm_mode_config_reset(drm);

  dri->kms = kms;
  dev_info(&dev->vdev->dev,
           "conduit-gpu: KMS head %ux%u@%u on %s, %d modifiers "
           "(kind %#x gen %u sector %u)%s\n",
           kms->width, kms->height, kms->refresh_mhz / 1000, dri->name,
           kms->dri->dev_info.v[NVGPU_DI_GENERIC_PAGE_KIND] ? 7 : 1,
           kms->dri->dev_info.v[NVGPU_DI_GENERIC_PAGE_KIND],
           kms->dri->dev_info.v[NVGPU_DI_PAGE_KIND_GENERATION],
           kms->dri->dev_info.v[NVGPU_DI_SECTOR_LAYOUT],
           kms->has_cursor ? ", cursor plane" : "");
  return 0;
}

/* ───────── input ───────── */

#define NVGPU_INPUT_ABS_MAX 0x7fff /* protocol INPUT_ABS_MAX: mode-independent */

#include "nvgpu_pad.h"

struct nvgpu_input {
  struct nvgpu_pads pads; /* gamepads from a stream client (nvgpu_pad.h) */
  struct input_dev *kbd;
  struct input_dev *mouse;  /* relative */
  struct input_dev *tablet; /* absolute */
  /* Event-queue interrupt state, which one callback at a time touches. */
  struct input_dev *last_ptr; /* where buttons and wheels go */
  unsigned int dirty;         /* bit 0 kbd, 1 mouse, 2 tablet */
};

static void nvgpu_input_set_pointer_caps(struct input_dev *in) {
  static const unsigned int btns[] = {BTN_LEFT, BTN_RIGHT, BTN_MIDDLE,
                                      BTN_SIDE, BTN_EXTRA, BTN_FORWARD,
                                      BTN_BACK, BTN_TASK};
  unsigned int i;

  __set_bit(EV_KEY, in->evbit);
  __set_bit(EV_REL, in->evbit);
  for (i = 0; i < ARRAY_SIZE(btns); i++)
    __set_bit(btns[i], in->keybit);
  __set_bit(REL_WHEEL, in->relbit);
  __set_bit(REL_HWHEEL, in->relbit);
  __set_bit(REL_WHEEL_HI_RES, in->relbit);
  __set_bit(REL_HWHEEL_HI_RES, in->relbit);
}

static struct input_dev *nvgpu_input_alloc(struct nvgpu_device *dev,
                                           const char *name, u16 product) {
  struct input_dev *in = input_allocate_device();

  if (!in)
    return NULL;
  in->name = name;
  in->phys = "conduit-gpu/input0";
  in->id.bustype = BUS_VIRTUAL;
  in->id.vendor = 0x10de;
  in->id.product = product;
  in->id.version = 1;
  in->dev.parent = &dev->vdev->dev;
  return in;
}

static void nvgpu_input_free(struct nvgpu_input *in) {
  struct input_dev **devs[] = {&in->kbd, &in->mouse, &in->tablet};
  unsigned int i;

  nvgpu_pads_free(&in->pads);
  for (i = 0; i < ARRAY_SIZE(devs); i++) {
    if (*devs[i])
      input_unregister_device(*devs[i]);
    *devs[i] = NULL;
  }
  kfree(in);
}

static int nvgpu_input_create(struct nvgpu_device *dev) {
  struct nvgpu_input *in;
  unsigned int i;
  int ret = -ENOMEM;

  in = kzalloc(sizeof(*in), GFP_KERNEL);
  if (!in)
    return -ENOMEM;
  nvgpu_pads_init(&in->pads, &dev->vdev->dev);

  /* Separate devices, because libinput classifies a device by what it has:
   * relative and absolute axes on one device make neither work well. */
  in->kbd = nvgpu_input_alloc(dev, "conduit-gpu keyboard", 1);
  if (!in->kbd)
    goto err;
  __set_bit(EV_KEY, in->kbd->evbit);
  for (i = KEY_ESC; i < BTN_MISC; i++)
    __set_bit(i, in->kbd->keybit);
  for (i = KEY_OK; i < BTN_TRIGGER_HAPPY; i++)
    __set_bit(i, in->kbd->keybit);
  ret = input_register_device(in->kbd);
  if (ret) {
    input_free_device(in->kbd);
    in->kbd = NULL;
    goto err;
  }

  ret = -ENOMEM;
  in->mouse = nvgpu_input_alloc(dev, "conduit-gpu mouse", 2);
  if (!in->mouse)
    goto err;
  nvgpu_input_set_pointer_caps(in->mouse);
  __set_bit(REL_X, in->mouse->relbit);
  __set_bit(REL_Y, in->mouse->relbit);
  ret = input_register_device(in->mouse);
  if (ret) {
    input_free_device(in->mouse);
    in->mouse = NULL;
    goto err;
  }

  ret = -ENOMEM;
  in->tablet = nvgpu_input_alloc(dev, "conduit-gpu tablet", 3);
  if (!in->tablet)
    goto err;
  nvgpu_input_set_pointer_caps(in->tablet);
  input_set_abs_params(in->tablet, ABS_X, 0, NVGPU_INPUT_ABS_MAX, 0, 0);
  input_set_abs_params(in->tablet, ABS_Y, 0, NVGPU_INPUT_ABS_MAX, 0, 0);
  ret = input_register_device(in->tablet);
  if (ret) {
    input_free_device(in->tablet);
    in->tablet = NULL;
    goto err;
  }

  in->last_ptr = in->tablet;
  /* Published last, once every device it names is registered. */
  smp_store_release(&dev->input, in);
  return 0;

err:
  nvgpu_input_free(in);
  return ret;
}

static void nvgpu_input_destroy(struct nvgpu_device *dev) {
  struct nvgpu_input *in = READ_ONCE(dev->input);

  if (!in)
    return;
  WRITE_ONCE(dev->input, NULL);
  /* The event callback runs in interrupt context with the pointer read
   * inside an RCU read section; after this none still holds it. */
  synchronize_rcu();
  nvgpu_input_free(in);
}

static void nvgpu_input_route(struct nvgpu_input *in, u16 type, u16 code,
                              s32 value) {
  struct input_dev *to = NULL;

  if (type >> 8) { /* a gamepad: (pad + 1) << 8 | type */
    nvgpu_pad_event(&in->pads, type >> 8, type & 0xff, code, value);
    return;
  }

  switch (type) {
  case EV_SYN:
    if (code != SYN_REPORT)
      return;
    if (in->dirty & 1)
      input_sync(in->kbd);
    if (in->dirty & 2)
      input_sync(in->mouse);
    if (in->dirty & 4)
      input_sync(in->tablet);
    in->dirty = 0;
    return;
  case EV_KEY:
    if (code >= BTN_MOUSE && code <= BTN_TASK)
      to = in->last_ptr;
    else if (code < KEY_CNT)
      to = in->kbd;
    break;
  case EV_REL:
    if (code == REL_X || code == REL_Y) {
      to = in->mouse;
      in->last_ptr = in->mouse;
    } else {
      to = in->last_ptr;
    }
    break;
  case EV_ABS:
    if (code == ABS_X || code == ABS_Y) {
      to = in->tablet;
      in->last_ptr = in->tablet;
    }
    break;
  default:
    break;
  }
  if (!to)
    return;
  input_event(to, type, code, value);
  in->dirty |= to == in->kbd ? 1 : to == in->mouse ? 2 : 4;
}

/*
 * An InputEvent batch, straight from the event-queue interrupt: { u32 count,
 * u32 pad } and `count` { u16 type, u16 code, s32 value } entries. Injected
 * here and now -- input_event() is interrupt-safe, and a work item would only
 * add a scheduling delay to every keystroke.
 */
static void nvgpu_input_batch(struct nvgpu_device *dev, const u8 *p,
                              unsigned int len) {
  struct nvgpu_input *in;
  u32 count, i;

  if (len < 8)
    return;
  count = get_unaligned_le32(p);
  count = min_t(u32, count, (len - 8) / 8);

  rcu_read_lock();
  in = smp_load_acquire(&dev->input);
  if (in) {
    for (i = 0; i < count; i++) {
      const u8 *e = p + 8 + 8 * i;

      nvgpu_input_route(in, get_unaligned_le16(e), get_unaligned_le16(e + 2),
                        (s32)get_unaligned_le32(e + 4));
    }
  }
  rcu_read_unlock();
}

/* ───────── lifetime ───────── */

/* After drm_dev_register() succeeded. Input failing leaves the display. */
static void nvgpu_kms_activate(struct nvgpu_dri_dev *dri) {
  struct nvgpu_device *dev = dri->dev;
  int ret;

  /* DisplayMode events may arrive from here on. */
  rcu_assign_pointer(dev->kms_ev, dri->kms);

  if (READ_ONCE(dev->input))
    return;
  ret = nvgpu_input_create(dev);
  if (ret)
    dev_warn(&dev->vdev->dev,
             "conduit-gpu: input devices not registered: %d\n", ret);
}

/*
 * From nvgpu_remove(), before the device is reset: stop sending on a queue
 * that is about to go, and stop injecting into devices about to go.
 */
static void nvgpu_display_quiesce(struct nvgpu_device *dev) {
  int i;

  for (i = 0; i < dev->num_dri_devs && i < NVGPU_MAX_DRI_DEVS; i++)
    if (dev->dri_devs[i].kms)
      WRITE_ONCE(dev->dri_devs[i].kms->dead, true);
  /* No interrupt still holds the head after this; no work is left pending. */
  RCU_INIT_POINTER(dev->kms_ev, NULL);
  synchronize_rcu();
  for (i = 0; i < dev->num_dri_devs && i < NVGPU_MAX_DRI_DEVS; i++)
    if (dev->dri_devs[i].kms) {
      cancel_work_sync(&dev->dri_devs[i].kms->mode_work);
      cancel_delayed_work_sync(&dev->dri_devs[i].kms->replug_work);
    }
  nvgpu_input_destroy(dev);
}

/*
 * The last file open on the node has closed: nobody owns the display any
 * more, so turn it off, as a driver with fbdev emulation hands it back to
 * the console at this point.
 *
 * Not leaving it lit matters because of what the frame left on it holds. A
 * compositor that exits with DRM_IOCTL_MODE_CLOSEFB (mutter does) leaves its
 * last framebuffer on the plane, and from 6.16 a framebuffer holds a GEM
 * handle reference on its object -- which keeps the object's exported dma-buf
 * alive, and a dma-buf holds the module that exported it. The session ended
 * and conduit_gpu stayed in use, with no process holding anything.
 *
 * Called with dri->open_lock held. Not once the device is unplugged: remove()
 * shuts the head down itself.
 */
static void nvgpu_kms_lastclose(struct nvgpu_dri_dev *dri,
                                struct drm_device *drm) {
  struct drm_modeset_acquire_ctx ctx;
  int idx, ret;

  if (!dri->kms || !drm_dev_enter(drm, &idx))
    return;
  DRM_MODESET_LOCK_ALL_BEGIN(drm, ctx, 0, ret);
  ret = drm_atomic_helper_disable_all(drm, &ctx);
  DRM_MODESET_LOCK_ALL_END(drm, ctx, ret);
  if (ret)
    dev_warn(&dri->dev->vdev->dev,
             "conduit-gpu: display not turned off after the last close: %d\n",
             ret);
  drm_dev_exit(idx);
}

/* After drm_dev_unplug(), before the last drm_dev_put(). */
static void nvgpu_kms_fini(struct nvgpu_dri_dev *dri) {
  struct nvgpu_kms *kms = dri->kms;

  if (!kms)
    return;
  if (rcu_access_pointer(dri->dev->kms_ev) == kms) {
    RCU_INIT_POINTER(dri->dev->kms_ev, NULL);
    synchronize_rcu();
  }
  cancel_work_sync(&kms->mode_work);
  cancel_delayed_work_sync(&kms->replug_work);
  /* Turns the CRTC off: vblank timer stopped, pending events sent. */
  drm_atomic_helper_shutdown(dri->drm);
#ifndef NVGPU_HAVE_VBLANK_TIMER
  nvgpu_vblank_timer_fini(&kms->vblank);
#endif
  dev_info(&dri->dev->vdev->dev,
           "conduit-gpu: KMS head down after %llu flips (%llu dropped), "
           "%llu cursor updates\n",
           kms->n_flips, kms->n_flip_errors, kms->n_cursor);
  nvgpu_input_destroy(dri->dev);
  dri->kms = NULL;
}
