/* SPDX-License-Identifier: GPL-2.0 */
/*
 * nvgpu_pcimap.h -- where the host's GPUs appear on the guest's PCI tree, and
 * the one translation between the two addresses.
 *
 * NVIDIA's userspace insists on finding the GPU on PCI: libdrm's drmGetDevice
 * reads the vendor of the DRM node's parent, egl-wayland2 refuses a device
 * whose vendor is not 0x10de, and NVML and the GL/Vulkan drivers match what
 * RM reports (NV_ESC_CARD_INFO, NV0000_CTRL_CMD_GPU_GET_PCI_INFO,
 * /proc/driver/nvidia/gpus/<addr>) against /sys/bus/pci/devices/<addr>. So
 * the module builds a PCI root bus of its own holding a device that answers
 * with the host GPU's config space.
 *
 * That bus used to sit at the host GPU's own address, which is only free when
 * the guest happens not to use it. A libvirt q35 guest puts its first PCIe
 * root port's device at 0000:01:00.0 -- the same address as a typical host
 * GPU -- and the mirror bus then failed with -EEXIST, leaving the DRM device
 * parented to the virtio device (vendor 0x1af4) and every EGL Wayland client
 * without a display.
 *
 * So the mirror never shares a domain with the guest. Each host PCI domain
 * that holds a GPU is given a guest domain of its own, the first ones at or
 * above NVGPU_PCI_GUEST_DOMAIN_BASE that no root bus of the guest uses; bus,
 * device and function stay the host's. Only the domain differs, so:
 *
 *   - no layout of the guest's can collide with the mirror, because the
 *     domain is chosen against the buses the guest actually has;
 *   - every translation is the domain alone, which is also all that the
 *     domain-only fields RM reports (NV2080_CTRL_BUS_INFO_INDEX_DOMAIN_NUMBER)
 *     carry -- they can be translated without knowing which GPU is meant;
 *   - two host GPUs keep their relative topology, and bus/device stay what
 *     the low 16 bits of RM's gpuId encode.
 *
 * RM, and therefore everything the backend forwards, speaks host addresses.
 * Everything userspace in the guest sees speaks guest addresses: the module
 * translates at the boundary, here, and nowhere else.
 *
 * gpuId is deliberately not translated. RM derives it from the host address,
 * but it is an opaque key: every place userspace obtains one is RM (probed and
 * attached ids, CARD_INFO, nvidia-drm's GET_DEV_INFO) and every place it is
 * consumed is RM or NVKMS again, which know only the host's. Rewriting it
 * would mean rewriting every control that takes one, in both directions, for
 * no consumer that decodes it.
 *
 * Plain C with no kernel dependencies beyond the basic types, so the same code
 * is exercised by guest/linux/test/pcimap_test.c on the build host.
 */
#ifndef NVGPU_PCIMAP_H
#define NVGPU_PCIMAP_H

#ifdef __KERNEL__
#include <linux/kernel.h>
#include <linux/string.h>
#include <linux/types.h>
#else
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
typedef uint8_t u8;
typedef uint16_t u16;
typedef uint32_t u32;
#endif

/*
 * Firmware numbers its segments from 0 and QEMU's q35 only ever has segment 0;
 * 0x10 leaves room for any multi-segment layout a VM is likely to have, and
 * is chosen against the buses that exist in any case. Below 0x10000, which is
 * where VMD starts, and within the 16 bits RM and "%04x" assume.
 */
#define NVGPU_PCI_GUEST_DOMAIN_BASE 0x10u
#define NVGPU_PCI_GUEST_DOMAIN_LIMIT 0xffffu
#define NVGPU_PCIMAP_MAX 8
#define NVGPU_PCI_ADDR_LEN 16

struct nvgpu_pcimap_gpu {
  u32 host_domain;
  u32 guest_domain;
  u8 bus, slot, func;
  char host_addr[NVGPU_PCI_ADDR_LEN];  /* "0000:01:00.0" */
  char guest_addr[NVGPU_PCI_ADDR_LEN]; /* "0010:01:00.0" */
};

struct nvgpu_pcimap {
  struct nvgpu_pcimap_gpu gpu[NVGPU_PCIMAP_MAX];
  int num;
};

/* Whether a domain already has a bus in the guest. */
typedef bool (*nvgpu_pci_domain_in_use_fn)(u32 domain, void *ctx);

static inline int nvgpu_pci_hex(char c) {
  if (c >= '0' && c <= '9')
    return c - '0';
  if (c >= 'a' && c <= 'f')
    return c - 'a' + 10;
  if (c >= 'A' && c <= 'F')
    return c - 'A' + 10;
  return -1;
}

/* Up to `max` hex digits, then `stop`: the digits consumed, or -1. */
static inline int nvgpu_pci_hexfield(const char *s, int max, char stop,
                                     u32 *out) {
  u32 v = 0;
  int n = 0;

  while (n < max && nvgpu_pci_hex(s[n]) >= 0)
    v = (v << 4) | (u32)nvgpu_pci_hex(s[n++]);
  if (n == 0 || s[n] != stop)
    return -1;
  *out = v;
  return n;
}

/*
 * Parse "DDDD:BB:SS.F" as Linux names a PCI device. The domain takes up to
 * eight digits: a host GPU behind VMD lives in domain 0x10000 or above.
 */
static inline int nvgpu_pci_parse(const char *s, u32 *domain, u8 *bus, u8 *slot,
                                  u8 *func) {
  u32 d, b, sl, f;
  int n;

  n = nvgpu_pci_hexfield(s, 8, ':', &d);
  if (n < 0)
    return -1;
  s += n + 1;
  n = nvgpu_pci_hexfield(s, 2, ':', &b);
  if (n < 0)
    return -1;
  s += n + 1;
  n = nvgpu_pci_hexfield(s, 2, '.', &sl);
  if (n < 0 || sl > 0x1f)
    return -1;
  s += n + 1;
  n = nvgpu_pci_hexfield(s, 1, '\0', &f);
  if (n < 0 || f > 7)
    return -1;
  *domain = d;
  *bus = (u8)b;
  *slot = (u8)sl;
  *func = (u8)f;
  return 0;
}

static inline void nvgpu_pci_format(char out[NVGPU_PCI_ADDR_LEN], u32 domain,
                                    u8 bus, u8 slot, u8 func) {
  snprintf(out, NVGPU_PCI_ADDR_LEN, "%04x:%02x:%02x.%x", domain, bus, slot,
           func);
}

/* The guest domain a host domain was given, or -1 if it holds no GPU. */
static inline long nvgpu_pcimap_guest_domain(const struct nvgpu_pcimap *m,
                                             u32 host_domain) {
  int i;

  for (i = 0; i < m->num; i++)
    if (m->gpu[i].host_domain == host_domain)
      return m->gpu[i].guest_domain;
  return -1;
}

static inline long nvgpu_pcimap_host_domain(const struct nvgpu_pcimap *m,
                                            u32 guest_domain) {
  int i;

  for (i = 0; i < m->num; i++)
    if (m->gpu[i].guest_domain == guest_domain)
      return m->gpu[i].host_domain;
  return -1;
}

/*
 * Build the map from the host addresses the backend announced, in slot order.
 * Every host domain gets the lowest guest domain at or above the base that
 * the guest does not use and no earlier host domain was given. Returns the
 * number of GPUs mapped; a slot that does not parse, or for which no domain
 * is left, is left out (its index in `gpu[]` is then not the slot's -- see
 * nvgpu_pcimap_find_host_addr).
 */
static inline int
nvgpu_pcimap_build(struct nvgpu_pcimap *m,
                   const char (*host_addrs)[NVGPU_PCI_ADDR_LEN], int n,
                   nvgpu_pci_domain_in_use_fn in_use, void *ctx) {
  u32 next = NVGPU_PCI_GUEST_DOMAIN_BASE;
  int i;

  memset(m, 0, sizeof(*m));
  for (i = 0; i < n && m->num < NVGPU_PCIMAP_MAX; i++) {
    struct nvgpu_pcimap_gpu *g = &m->gpu[m->num];
    char addr[NVGPU_PCI_ADDR_LEN];
    long have;

    memcpy(addr, host_addrs[i], sizeof(addr));
    addr[NVGPU_PCI_ADDR_LEN - 1] = '\0';
    if (nvgpu_pci_parse(addr, &g->host_domain, &g->bus, &g->slot, &g->func))
      continue;

    have = nvgpu_pcimap_guest_domain(m, g->host_domain);
    if (have >= 0) {
      g->guest_domain = (u32)have;
    } else {
      while (next <= NVGPU_PCI_GUEST_DOMAIN_LIMIT && in_use &&
             in_use(next, ctx))
        next++;
      if (next > NVGPU_PCI_GUEST_DOMAIN_LIMIT)
        break;
      g->guest_domain = next++;
    }
    /* The announced spelling, so that it matches the host's files byte for
     * byte; the guest's is formatted the way the kernel will name it. */
    memcpy(g->host_addr, addr, sizeof(g->host_addr));
    nvgpu_pci_format(g->guest_addr, g->guest_domain, g->bus, g->slot, g->func);
    m->num++;
  }
  return m->num;
}

static inline const struct nvgpu_pcimap_gpu *
nvgpu_pcimap_find_host_addr(const struct nvgpu_pcimap *m, const char *addr) {
  int i;

  for (i = 0; i < m->num; i++)
    if (strcmp(m->gpu[i].host_addr, addr) == 0)
      return &m->gpu[i];
  return NULL;
}

/* ── Text: /proc/driver/nvidia paths and contents ─────────────────────── */

/*
 * Replace every host address with its guest one, in place, and return the
 * new length. The guest's spelling is never longer than the host's (the guest
 * domain fits four digits, and Linux prints the host's with at least four), so
 * the text can only shrink; an address spelled shorter is left as it is. Only
 * whole addresses are replaced: a match must not continue a longer hex run on
 * either side.
 */
static inline size_t nvgpu_pcimap_text(const struct nvgpu_pcimap *m, char *buf,
                                       size_t len) {
  size_t r = 0, w = 0;

  while (r < len) {
    int i, hit = -1;
    size_t hl = 0;

    if (r == 0 || nvgpu_pci_hex(buf[r - 1]) < 0) {
      for (i = 0; i < m->num; i++) {
        hl = strlen(m->gpu[i].host_addr);
        /* Never longer: in place, the write must not overtake the read. */
        if (hl && strlen(m->gpu[i].guest_addr) <= hl && r + hl <= len &&
            memcmp(buf + r, m->gpu[i].host_addr, hl) == 0 &&
            (r + hl == len || nvgpu_pci_hex(buf[r + hl]) < 0)) {
          hit = i;
          break;
        }
      }
    }
    if (hit >= 0) {
      size_t gl = strlen(m->gpu[hit].guest_addr);

      memmove(buf + w, m->gpu[hit].guest_addr, gl);
      w += gl;
      r += hl;
    } else {
      buf[w++] = buf[r++];
    }
  }
  return w;
}

/* ── Binary: escapes and RM controls ──────────────────────────────────── */

static inline u32 nvgpu_pcimap_rd32(const u8 *p) {
  return (u32)p[0] | (u32)p[1] << 8 | (u32)p[2] << 16 | (u32)p[3] << 24;
}

static inline void nvgpu_pcimap_wr32(u8 *p, u32 v) {
  p[0] = (u8)v;
  p[1] = (u8)(v >> 8);
  p[2] = (u8)(v >> 16);
  p[3] = (u8)(v >> 24);
}

/* A u32 domain at p: host -> guest when `to_guest`, else guest -> host. */
static inline bool nvgpu_pcimap_domain32(const struct nvgpu_pcimap *m, u8 *p,
                                         bool to_guest) {
  u32 d = nvgpu_pcimap_rd32(p);
  long x = to_guest ? nvgpu_pcimap_guest_domain(m, d)
                    : nvgpu_pcimap_host_domain(m, d);

  if (x < 0)
    return false;
  nvgpu_pcimap_wr32(p, (u32)x);
  return true;
}

/* A dbdf word: domain 31:16, bus 15:8, devfn 7:0. */
static inline bool nvgpu_pcimap_dbdf(const struct nvgpu_pcimap *m, u8 *p,
                                     bool to_guest) {
  u32 v = nvgpu_pcimap_rd32(p);
  long x = to_guest ? nvgpu_pcimap_guest_domain(m, v >> 16)
                    : nvgpu_pcimap_host_domain(m, v >> 16);

  if (x < 0 || x > 0xffff)
    return false;
  nvgpu_pcimap_wr32(p, ((u32)x << 16) | (v & 0xffff));
  return true;
}

/*
 * NV_ESC_CARD_INFO: an array of nv_ioctl_card_info_t, out only.
 *   NvBool valid @0; nv_pci_info_t pci_info @4 { NvU32 domain @4; ... }
 * 72 bytes each, unchanged since the struct was introduced (and as gVisor's
 * nvproxy declares it).
 */
#define NVGPU_CARD_INFO_SIZE 72
#define NVGPU_CARD_INFO_VALID 0
#define NVGPU_CARD_INFO_DOMAIN 4

static inline int nvgpu_pcimap_card_info(const struct nvgpu_pcimap *m, u8 *buf,
                                         size_t len) {
  size_t at;
  int n = 0;

  for (at = 0; at + NVGPU_CARD_INFO_SIZE <= len; at += NVGPU_CARD_INFO_SIZE)
    if (buf[at + NVGPU_CARD_INFO_VALID] &&
        nvgpu_pcimap_domain32(m, buf + at + NVGPU_CARD_INFO_DOMAIN, true))
      n++;
  return n;
}

/*
 * NV_ESC_STATUS_CODE: nv_ioctl_status_code_t { NvU32 domain @0; NvU8 bus @4;
 * NvU8 slot @5; NvU32 status @8 }, 12 bytes. The caller names the GPU by the
 * address it was told, so it goes to RM as the host's; the caller's own words
 * are put back afterwards (nvgpu_pcimap_restore).
 */
#define NVGPU_STATUS_CODE_SIZE 12

/*
 * A field a request carries in, translated to the host's on the way to RM and
 * restored on the way back, so the caller never sees a host address even in
 * its own parameters.
 */
struct nvgpu_pcimap_saved {
  int at; /* byte offset of the rewritten u32, or -1 */
  u32 orig;
};

static inline void nvgpu_pcimap_restore(const struct nvgpu_pcimap_saved *s,
                                        u8 *p, size_t len) {
  if (s->at >= 0 && (size_t)s->at + 4 <= len)
    nvgpu_pcimap_wr32(p + s->at, s->orig);
}

static inline bool nvgpu_pcimap_save(struct nvgpu_pcimap_saved *s, int at,
                                     bool changed, u32 orig) {
  if (!changed)
    return false;
  s->at = at;
  s->orig = orig;
  return true;
}

static inline bool nvgpu_pcimap_status_code_in(const struct nvgpu_pcimap *m,
                                               u8 *buf, size_t len,
                                               struct nvgpu_pcimap_saved *s) {
  u32 orig;

  s->at = -1;
  if (len < NVGPU_STATUS_CODE_SIZE)
    return false;
  orig = nvgpu_pcimap_rd32(buf);
  return nvgpu_pcimap_save(s, 0, nvgpu_pcimap_domain32(m, buf, false), orig);
}

/*
 * RM controls that carry a PCI location, from a sweep of every ctrl header of
 * the release for domain/bus/BDF fields (src/common/sdk/nvidia/inc/ctrl):
 *
 *   0x0000021b NV0000_CTRL_CMD_GPU_GET_PCI_INFO          domain out    @4
 *   0x20801823 NV2080_CTRL_CMD_BUS_GET_INFO_V2           list, inline  @4
 *   0x20801802 NV2080_CTRL_CMD_BUS_GET_INFO              list, by NvP64 (the
 *              entries are translated where the pointer's segment returns)
 *   0x20801829 NV2080_CTRL_CMD_BUS_GET_PCIE_REQ_ATOMICS_CAPS   dbdf in @4
 *   0x2080182a NV2080_CTRL_CMD_BUS_GET_PCIE_SUPPORTED_GPU_ATOMICS dbdf in @4
 *
 * Deliberately not translated, and why:
 *   NV0000_CTRL_CMD_GPU_DISCOVER, _PCI_READ_WRITE: admin-only hot-add scan and
 *     MODS-only config access, naming host bridges the guest cannot see.
 *   NV2080_CTRL_CMD_GPU_GET_ALL_BRIDGES_UPSTREAM_OF_GPU, _GET_GFID, vGPU
 *     manager controls (gpuPciBdf, dbdf): host bridges and the host-side vGPU
 *     manager; there is nothing at those addresses in the guest.
 *   NV2080_CTRL_NVLINK_DEVICE_INFO (NVLink status/peer queries): the layout of
 *     the enclosing per-link arrays changes between releases; see the report.
 *   NV0000_CTRL_CMD_SYSTEM_GET_GPUS_POWER_STATUS: bus numbers only, which the
 *     mirror keeps.
 */
#define NVGPU_RM_GPU_GET_PCI_INFO 0x0000021bu
#define NVGPU_RM_GPU_GET_PCI_INFO_SIZE 12u
#define NVGPU_RM_BUS_GET_INFO 0x20801802u
#define NVGPU_RM_BUS_GET_INFO_V2 0x20801823u
#define NVGPU_RM_BUS_PCIE_REQ_ATOMICS_CAPS 0x20801829u
#define NVGPU_RM_BUS_PCIE_SUPPORTED_GPU_ATOMICS 0x2080182au
#define NVGPU_RM_BUS_INFO_INDEX_DOMAIN_NUMBER 0x2cu

/*
 * NV2080_CTRL_BUS_INFO entries {NvU32 index; NvU32 data;}: the domain is
 * reported as one entry among many, with no bus or device beside it.
 */
static inline int nvgpu_pcimap_bus_info_list(const struct nvgpu_pcimap *m,
                                             u8 *list, size_t len, u32 count,
                                             bool to_guest) {
  u32 i;
  int n = 0;

  for (i = 0; i < count && (size_t)(i + 1) * 8 <= len; i++)
    if (nvgpu_pcimap_rd32(list + i * 8) ==
            NVGPU_RM_BUS_INFO_INDEX_DOMAIN_NUMBER &&
        nvgpu_pcimap_domain32(m, list + i * 8 + 4, to_guest))
      n++;
  return n;
}

/* An answer on its way back from RM. Returns the number of fields rewritten. */
static inline int nvgpu_pcimap_rmctrl_out(const struct nvgpu_pcimap *m, u32 cmd,
                                          u8 *p, size_t len) {
  switch (cmd) {
  case NVGPU_RM_GPU_GET_PCI_INFO:
    /* {gpuId, domain, NvU16 bus, NvU16 slot}. */
    if (len == NVGPU_RM_GPU_GET_PCI_INFO_SIZE)
      return nvgpu_pcimap_domain32(m, p + 4, true);
    return 0;
  case NVGPU_RM_BUS_GET_INFO_V2:
    /* {busInfoListSize, entries[]}: the index says which is the domain. */
    if (len >= 4)
      return nvgpu_pcimap_bus_info_list(m, p + 4, len - 4, nvgpu_pcimap_rd32(p),
                                        true);
    return 0;
  default:
    return 0;
  }
}

/*
 * A request on its way to RM: a field naming a GPU by the guest's address is
 * rewritten to the host's and recorded in `s`, for nvgpu_pcimap_restore once
 * the answer is back.
 */
static inline bool nvgpu_pcimap_rmctrl_in(const struct nvgpu_pcimap *m, u32 cmd,
                                          u8 *p, size_t len,
                                          struct nvgpu_pcimap_saved *s) {
  u32 orig;

  s->at = -1;
  switch (cmd) {
  case NVGPU_RM_BUS_PCIE_REQ_ATOMICS_CAPS:
  case NVGPU_RM_BUS_PCIE_SUPPORTED_GPU_ATOMICS:
    /* {capType, dbdf, ...}: the peer endpoint the caller names. */
    if (len < 8)
      return false;
    orig = nvgpu_pcimap_rd32(p + 4);
    return nvgpu_pcimap_save(s, 4, nvgpu_pcimap_dbdf(m, p + 4, false), orig);
  default:
    return false;
  }
}

#endif /* NVGPU_PCIMAP_H */
