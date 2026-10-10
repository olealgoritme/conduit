/* SPDX-License-Identifier: GPL-2.0 */
/*
 * nvgpu_replug.h -- when a host mode change needs the connector replugged.
 *
 * A DisplayMode event makes its mode the connector's preferred one and fires
 * a hotplug event (nvgpu_kms.h). Compositors split on what they do with a
 * hotplug on a connector that stays connected:
 *
 *   - mutter, KWin and X11 desktops re-read the mode list, and with
 *     "hotplug_mode_update" apply the new preferred mode;
 *   - wlroots (Sway, river, labwc, ...) and aquamarine (Hyprland) re-read the
 *     list only when the connector goes disconnected -> connected, so they
 *     keep the old mode, and a "preferred" output rule never sees the new one.
 *
 * So the plain hotplug goes first, and the head is watched for a grace
 * period. If by then the CRTC still shows something other than the new
 * preferred mode, the connector is replugged: it reports disconnected and a
 * hotplug is sent; once the DRM master has probed it (it saw the disconnect)
 * or a timeout passed, it reports connected again and a second hotplug is
 * sent. The compositor then sets the output up anew from the fresh list and
 * applies its rule for it ("preferred" -> the host's mode).
 *
 * A compositor that followed the first hotplug never sees a disconnect. One
 * that deliberately keeps a fixed mode sees one replug per host change, then
 * applies its fixed mode again.
 *
 * A head that is off when the grace period ends (DPMS off: a lock screen or
 * an idle blank) shows no mode at all, and the compositor turns it back on at
 * whatever mode it kept, without probing. So the watch stays armed: the next
 * time the head comes on, the grace period starts again from there.
 *
 * Plain C, no libc: the state machine is the same in the module and in
 * test/replug_test.c. The caller serialises every call (the module holds
 * mode_config.mutex).
 */
#ifndef NVGPU_REPLUG_H
#define NVGPU_REPLUG_H

#ifdef __KERNEL__
#include <linux/types.h>
#else
#include <stdbool.h>
#include <stdint.h>
typedef uint32_t u32;
#endif

enum nvgpu_replug_stage {
  NVGPU_REPLUG_IDLE,
  /* A new preferred mode was announced; the grace period runs. */
  NVGPU_REPLUG_WATCH,
  /* The connector reports disconnected until the timer fires again. */
  NVGPU_REPLUG_OFF,
};

struct nvgpu_replug {
  enum nvgpu_replug_stage stage;
  /* In OFF: the DRM master probed the connector and saw it disconnected. */
  bool probed;
};

/* What the timer's run asks the caller to do. */
enum nvgpu_replug_act {
  NVGPU_REPLUG_NOTHING,
  /* Report disconnected (already so in the state), send a hotplug, and
   * run the timer again after the probe timeout. */
  NVGPU_REPLUG_DISCONNECT,
  /* Report connected (already so in the state) and send a hotplug. */
  NVGPU_REPLUG_RECONNECT,
};

/*
 * The preferred mode changed and its hotplug was sent. True: (re)start the
 * grace timer. During a replug nothing changes: the reconnect probe will read
 * the newest mode anyway.
 */
static inline bool nvgpu_replug_on_mode(struct nvgpu_replug *r) {
  if (r->stage == NVGPU_REPLUG_OFF)
    return false;
  r->stage = NVGPU_REPLUG_WATCH;
  return true;
}

/*
 * Whether a head that is on shows the preferred mode: same size, and a
 * refresh within a hertz (`cur_hz` is drm_mode_vrefresh()'s whole hertz).
 */
static inline bool nvgpu_replug_applied(u32 cur_w, u32 cur_h, u32 cur_hz,
                                        u32 want_w, u32 want_h, u32 want_mhz) {
  u32 cur_mhz = cur_hz * 1000;
  u32 diff;

  if (cur_w != want_w || cur_h != want_h)
    return false;
  diff = cur_mhz > want_mhz ? cur_mhz - want_mhz : want_mhz - cur_mhz;
  return diff < 1000;
}

/*
 * The timer ran. `head_on`: the head is lit (enabled and active); `applied`:
 * nvgpu_replug_applied() for it. A head that is off keeps the watch armed,
 * with no timer: the caller restarts the grace period when it comes on
 * (nvgpu_replug_watching()).
 */
static inline enum nvgpu_replug_act
nvgpu_replug_on_timer(struct nvgpu_replug *r, bool head_on, bool applied) {
  switch (r->stage) {
  case NVGPU_REPLUG_WATCH:
    if (!head_on)
      return NVGPU_REPLUG_NOTHING;
    if (applied) {
      r->stage = NVGPU_REPLUG_IDLE;
      return NVGPU_REPLUG_NOTHING;
    }
    r->stage = NVGPU_REPLUG_OFF;
    r->probed = false;
    return NVGPU_REPLUG_DISCONNECT;
  case NVGPU_REPLUG_OFF:
    r->stage = NVGPU_REPLUG_IDLE;
    return NVGPU_REPLUG_RECONNECT;
  case NVGPU_REPLUG_IDLE:
  default:
    return NVGPU_REPLUG_NOTHING;
  }
}

/* Whether a new preferred mode is still waiting to be seen on the head. */
static inline bool nvgpu_replug_watching(const struct nvgpu_replug *r) {
  return r->stage == NVGPU_REPLUG_WATCH;
}

/*
 * The connector is probed (detect). Returns whether it is connected. `*kick`:
 * this was the first probe that saw the disconnect, so the reconnect may run
 * shortly instead of at the timeout.
 */
static inline bool nvgpu_replug_on_probe(struct nvgpu_replug *r, bool *kick) {
  *kick = false;
  if (r->stage != NVGPU_REPLUG_OFF)
    return true;
  if (!r->probed) {
    r->probed = true;
    *kick = true;
  }
  return false;
}

#endif /* NVGPU_REPLUG_H */
