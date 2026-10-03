/* SPDX-License-Identifier: GPL-2.0 */
/*
 * nvgpu_pad.h -- gamepads for the guest (input from a stream client).
 *
 * The host sends gamepad input in the ordinary InputEvent batches, with the
 * pad number in the high byte of the event type: type = (pad + 1) << 8 |
 * EV_KEY / EV_ABS / EV_SYN (docs/STREAMING.md). Older guests drop those
 * types, so the host can send them unconditionally.
 *
 * A pad's input device appears when its first event arrives. Registering an
 * input device sleeps and events arrive in interrupt context, so the first
 * event only schedules the registration; events before it is done are
 * dropped (the client sends full state again on the next change).
 *
 * Xbox 360 layout and ids (045e:028e on USB), which is what games and SDL
 * map without configuration.
 */
#ifndef NVGPU_PAD_H
#define NVGPU_PAD_H

#include <linux/input.h>
#include <linux/workqueue.h>

#define NVGPU_PADS 4

struct nvgpu_pads {
  struct device *parent;
  struct input_dev *pad[NVGPU_PADS]; /* published once registered */
  unsigned long want;                /* pads asked for, not yet registered */
  struct work_struct work;
  bool dying;
};

static const unsigned int nvgpu_pad_buttons[] = {
    BTN_SOUTH, BTN_EAST, BTN_NORTH, BTN_WEST, BTN_TL, BTN_TR,
    BTN_SELECT, BTN_START, BTN_MODE, BTN_THUMBL, BTN_THUMBR,
    BTN_TRIGGER_HAPPY1, BTN_TRIGGER_HAPPY2, BTN_TRIGGER_HAPPY3,
    BTN_TRIGGER_HAPPY4, BTN_TRIGGER_HAPPY5, BTN_TRIGGER_HAPPY6,
};

static const char *const nvgpu_pad_names[NVGPU_PADS] = {
    "Conduit gamepad 1", "Conduit gamepad 2", "Conduit gamepad 3",
    "Conduit gamepad 4",
};

static struct input_dev *nvgpu_pad_new(struct nvgpu_pads *p, unsigned int i) {
  struct input_dev *in = input_allocate_device();
  unsigned int b;

  if (!in)
    return NULL;
  in->name = nvgpu_pad_names[i];
  in->phys = "virtio-gpu-nv/pad";
  in->id.bustype = BUS_USB;
  in->id.vendor = 0x045e;  /* the Xbox 360 controller's ids: every game */
  in->id.product = 0x028e; /* and SDL know its layout */
  in->id.version = 0x0114;
  in->dev.parent = p->parent;
  for (b = 0; b < ARRAY_SIZE(nvgpu_pad_buttons); b++)
    input_set_capability(in, EV_KEY, nvgpu_pad_buttons[b]);
  input_set_abs_params(in, ABS_X, -32768, 32767, 16, 128);
  input_set_abs_params(in, ABS_Y, -32768, 32767, 16, 128);
  input_set_abs_params(in, ABS_RX, -32768, 32767, 16, 128);
  input_set_abs_params(in, ABS_RY, -32768, 32767, 16, 128);
  input_set_abs_params(in, ABS_Z, 0, 255, 0, 0);
  input_set_abs_params(in, ABS_RZ, 0, 255, 0, 0);
  input_set_abs_params(in, ABS_HAT0X, -1, 1, 0, 0);
  input_set_abs_params(in, ABS_HAT0Y, -1, 1, 0, 0);
  if (input_register_device(in)) {
    input_free_device(in);
    return NULL;
  }
  return in;
}

static void nvgpu_pad_work(struct work_struct *w) {
  struct nvgpu_pads *p = container_of(w, struct nvgpu_pads, work);
  unsigned int i;

  for (i = 0; i < NVGPU_PADS; i++) {
    struct input_dev *in;

    if (READ_ONCE(p->dying))
      return;
    if (!test_bit(i, &p->want) || READ_ONCE(p->pad[i]))
      continue;
    in = nvgpu_pad_new(p, i);
    if (in)
      smp_store_release(&p->pad[i], in);
    else
      clear_bit(i, &p->want); /* try again on the next event */
  }
}

static void nvgpu_pads_init(struct nvgpu_pads *p, struct device *parent) {
  memset(p, 0, sizeof(*p));
  p->parent = parent;
  INIT_WORK(&p->work, nvgpu_pad_work);
}

/* From the event-queue interrupt. `pad1` is the type's high byte (pad + 1). */
static void nvgpu_pad_event(struct nvgpu_pads *p, unsigned int pad1, u16 type,
                            u16 code, s32 value) {
  struct input_dev *in;
  unsigned int i = pad1 - 1;

  if (pad1 == 0 || i >= NVGPU_PADS)
    return;
  in = smp_load_acquire(&p->pad[i]);
  if (!in) {
    if (!READ_ONCE(p->dying) && !test_and_set_bit(i, &p->want))
      schedule_work(&p->work);
    return;
  }
  switch (type) {
  case EV_SYN:
    if (code == SYN_REPORT)
      input_sync(in);
    break;
  case EV_KEY:
  case EV_ABS:
    input_event(in, type, code, value); /* the core drops codes not set above */
    break;
  default:
    break;
  }
}

/* No events may be in flight any more (the caller synchronised that). */
static void nvgpu_pads_free(struct nvgpu_pads *p) {
  unsigned int i;

  WRITE_ONCE(p->dying, true);
  cancel_work_sync(&p->work);
  for (i = 0; i < NVGPU_PADS; i++) {
    if (p->pad[i])
      input_unregister_device(p->pad[i]);
    p->pad[i] = NULL;
  }
}

#endif
