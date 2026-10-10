/* SPDX-License-Identifier: GPL-2.0 */
/*
 * nvgpu_modelist.h -- the host's mode list (docs/SCANOUT.md "Mode list").
 *
 * The backend sends the display's whole mode list as one DisplayModeList
 * event: native first, then the standard modes up to native and the VM's
 * custom modes, all at one refresh rate (protocol::modes). The KMS head
 * offers exactly these, with the current preferred mode (DisplayMode) in
 * front, the way a monitor's EDID lists its modes.
 *
 * Payload, little-endian, after the message header:
 *   u32 scanout (0), u32 count, u32 refresh_mhz, u32 flags (0),
 *   then count x { u32 width, u32 height }.
 *
 * Plain C, no libc: parsed the same way in the module and in
 * test/modelist_test.c.
 */
#ifndef NVGPU_MODELIST_H
#define NVGPU_MODELIST_H

#ifdef __KERNEL__
#include <linux/types.h>
#else
#include <stdbool.h>
#include <stdint.h>
typedef uint8_t u8;
typedef uint32_t u32;
#endif

/* protocol::modes::MODE_LIST_MAX */
#define NVGPU_MODELIST_MAX 48
#define NVGPU_MODELIST_HDR 16

struct nvgpu_modelist {
  u32 count;
  u32 refresh_mhz; /* 0: the head's current rate */
  u32 w[NVGPU_MODELIST_MAX];
  u32 h[NVGPU_MODELIST_MAX];
};

static inline u32 nvgpu_modelist_le32(const u8 *p) {
  return (u32)p[0] | (u32)p[1] << 8 | (u32)p[2] << 16 | (u32)p[3] << 24;
}

/*
 * Parse a DisplayModeList payload of `len` bytes into `out`. Entries outside
 * min_dim..max_dim, and repeats, are skipped; a refresh outside 1..max_hz Hz
 * reads as 0. False (and `out` untouched) for another scanout, a count over
 * the cap, a payload shorter than its count, or no usable entry.
 */
static inline bool nvgpu_modelist_parse(const u8 *p, unsigned int len,
                                        u32 min_dim, u32 max_dim, u32 max_hz,
                                        struct nvgpu_modelist *out) {
  struct nvgpu_modelist l;
  u32 count, i, j;

  if (len < NVGPU_MODELIST_HDR || nvgpu_modelist_le32(p) != 0)
    return false;
  count = nvgpu_modelist_le32(p + 4);
  if (count == 0 || count > NVGPU_MODELIST_MAX ||
      len < NVGPU_MODELIST_HDR + 8 * count)
    return false;
  l.refresh_mhz = nvgpu_modelist_le32(p + 8);
  if (l.refresh_mhz < 1000 || l.refresh_mhz > max_hz * 1000)
    l.refresh_mhz = 0;
  l.count = 0;
  for (i = 0; i < count; i++) {
    u32 w = nvgpu_modelist_le32(p + NVGPU_MODELIST_HDR + 8 * i);
    u32 h = nvgpu_modelist_le32(p + NVGPU_MODELIST_HDR + 8 * i + 4);
    bool dup = false;

    if (w < min_dim || h < min_dim || w > max_dim || h > max_dim)
      continue;
    for (j = 0; j < l.count; j++)
      if (l.w[j] == w && l.h[j] == h)
        dup = true;
    if (dup)
      continue;
    l.w[l.count] = w;
    l.h[l.count] = h;
    l.count++;
  }
  if (!l.count)
    return false;
  *out = l;
  return true;
}

/* Whether two lists offer the same modes in the same order. */
static inline bool nvgpu_modelist_equal(const struct nvgpu_modelist *a,
                                        const struct nvgpu_modelist *b) {
  u32 i;

  if (a->count != b->count || a->refresh_mhz != b->refresh_mhz)
    return false;
  for (i = 0; i < a->count; i++)
    if (a->w[i] != b->w[i] || a->h[i] != b->h[i])
      return false;
  return true;
}

#endif /* NVGPU_MODELIST_H */
