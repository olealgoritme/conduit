// SPDX-License-Identifier: GPL-2.0
/*
 * Unit tests for nvgpu_pcimap.h, built and run on the build host:
 *
 *     make -C guest/linux check
 */
#include "../nvgpu_pcimap.h"

#include <stdlib.h>

static int failures;

#define CHECK(cond)                                                            \
  do {                                                                         \
    if (!(cond)) {                                                             \
      fprintf(stderr, "%s:%d: CHECK failed: %s\n", __FILE__, __LINE__, #cond); \
      failures++;                                                              \
    }                                                                          \
  } while (0)

/* A guest whose root buses are in the domains listed in `ctx`, -1-terminated.
 */
static bool in_use(u32 domain, void *ctx) {
  const long *d;

  for (d = ctx; d && *d >= 0; d++)
    if ((u32)*d == domain)
      return true;
  return false;
}

static void build(struct nvgpu_pcimap *m, const char *const *addrs, int n,
                  const long *guest_domains) {
  char a[NVGPU_PCIMAP_MAX][NVGPU_PCI_ADDR_LEN];
  int i;

  memset(a, 0, sizeof(a));
  for (i = 0; i < n; i++)
    strncpy(a[i], addrs[i], NVGPU_PCI_ADDR_LEN - 1);
  nvgpu_pcimap_build(m, (const char (*)[NVGPU_PCI_ADDR_LEN])a, n, in_use,
                     (void *)guest_domains);
}

static void test_parse(void) {
  u32 d;
  u8 b, s, f;

  CHECK(nvgpu_pci_parse("0000:01:00.0", &d, &b, &s, &f) == 0);
  CHECK(d == 0 && b == 1 && s == 0 && f == 0);
  CHECK(nvgpu_pci_parse("0001:c1:1f.7", &d, &b, &s, &f) == 0);
  CHECK(d == 1 && b == 0xc1 && s == 0x1f && f == 7);
  /* A host GPU behind VMD has a five-digit domain. */
  CHECK(nvgpu_pci_parse("10000:e1:00.0", &d, &b, &s, &f) == 0);
  CHECK(d == 0x10000 && b == 0xe1);
  CHECK(nvgpu_pci_parse("0000:01:20.0", &d, &b, &s, &f) != 0); /* slot > 31 */
  CHECK(nvgpu_pci_parse("0000:01:00.8", &d, &b, &s, &f) != 0); /* func > 7 */
  CHECK(nvgpu_pci_parse("0000:01:00", &d, &b, &s, &f) != 0);
  CHECK(nvgpu_pci_parse("0000:01:00.0x", &d, &b, &s, &f) != 0);
  CHECK(nvgpu_pci_parse("", &d, &b, &s, &f) != 0);
}

/* The Omarchy case: virtio-net is at the host GPU's own address. */
static void test_build_q35(void) {
  static const char *const addrs[] = {"0000:01:00.0"};
  static const long guest[] = {0, -1};
  struct nvgpu_pcimap m;

  build(&m, addrs, 1, guest);
  CHECK(m.num == 1);
  CHECK(m.gpu[0].host_domain == 0 && m.gpu[0].guest_domain == 0x10);
  CHECK(strcmp(m.gpu[0].host_addr, "0000:01:00.0") == 0);
  CHECK(strcmp(m.gpu[0].guest_addr, "0010:01:00.0") == 0);
  CHECK(nvgpu_pcimap_find_host_addr(&m, "0000:01:00.0") == &m.gpu[0]);
  CHECK(nvgpu_pcimap_find_host_addr(&m, "0010:01:00.0") == NULL);
}

/* A guest already using 0x10 and 0x11 gets the next free one. */
static void test_build_skips_used(void) {
  static const char *const addrs[] = {"0000:01:00.0"};
  static const long guest[] = {0, 0x10, 0x11, 0x13, -1};
  struct nvgpu_pcimap m;

  build(&m, addrs, 1, guest);
  CHECK(m.num == 1 && m.gpu[0].guest_domain == 0x12);
  CHECK(strcmp(m.gpu[0].guest_addr, "0012:01:00.0") == 0);
}

/* GPUs sharing a host domain share a guest one; another host domain gets its
 * own; a slot that does not parse is left out. */
static void test_build_multi(void) {
  static const char *const addrs[] = {"0000:01:00.0", "0000:41:00.0", "bogus",
                                      "0002:01:00.0", "10000:e1:00.0"};
  static const long guest[] = {0, 0x11, -1};
  struct nvgpu_pcimap m;

  build(&m, addrs, 5, guest);
  CHECK(m.num == 4);
  CHECK(m.gpu[0].guest_domain == 0x10 && m.gpu[1].guest_domain == 0x10);
  CHECK(m.gpu[2].host_domain == 2 && m.gpu[2].guest_domain == 0x12);
  CHECK(strcmp(m.gpu[2].guest_addr, "0012:01:00.0") == 0);
  CHECK(m.gpu[3].host_domain == 0x10000 && m.gpu[3].guest_domain == 0x13);
  CHECK(strcmp(m.gpu[3].guest_addr, "0013:e1:00.0") == 0);
  CHECK(nvgpu_pcimap_guest_domain(&m, 0) == 0x10);
  CHECK(nvgpu_pcimap_guest_domain(&m, 1) == -1);
  CHECK(nvgpu_pcimap_host_domain(&m, 0x12) == 2);
  CHECK(nvgpu_pcimap_host_domain(&m, 0) == -1);
}

static void test_text(void) {
  static const char *const addrs[] = {"0000:01:00.0", "10000:e1:00.0"};
  static const long guest[] = {0, -1};
  struct nvgpu_pcimap m;
  char buf[256];
  size_t n;

  build(&m, addrs, 2, guest);

  strcpy(buf, "driver/nvidia/gpus/0000:01:00.0/information");
  n = nvgpu_pcimap_text(&m, buf, strlen(buf));
  buf[n] = '\0';
  CHECK(strcmp(buf, "driver/nvidia/gpus/0010:01:00.0/information") == 0);

  strcpy(buf, "Model: \t\t NVIDIA GeForce RTX 5090\n"
              "Bus Location: \t 0000:01:00.0\nDevice Minor: \t 0\n");
  n = nvgpu_pcimap_text(&m, buf, strlen(buf));
  buf[n] = '\0';
  CHECK(strstr(buf, "Bus Location: \t 0010:01:00.0\nDevice Minor") != NULL);
  CHECK(strstr(buf, "0000:01:00.0") == NULL);

  /* Shrinks where the host's domain had five digits. */
  strcpy(buf, "gpus/10000:e1:00.0/x 10000:e1:00.0");
  n = nvgpu_pcimap_text(&m, buf, strlen(buf));
  buf[n] = '\0';
  CHECK(strcmp(buf, "gpus/0011:e1:00.0/x 0011:e1:00.0") == 0);

  /* Not part of a longer hex run, and not other devices. */
  strcpy(buf, "a0000:01:00.0 0000:01:00.01 0000:02:00.0");
  n = nvgpu_pcimap_text(&m, buf, strlen(buf));
  buf[n] = '\0';
  CHECK(strcmp(buf, "a0000:01:00.0 0000:01:00.01 0000:02:00.0") == 0);

  /* A host spelling shorter than the guest's is never replaced in place. */
  {
    static const char *const short_addrs[] = {"0:01:00.0"};
    struct nvgpu_pcimap sm;

    build(&sm, short_addrs, 1, guest);
    CHECK(sm.num == 1 && strcmp(sm.gpu[0].guest_addr, "0010:01:00.0") == 0);
    strcpy(buf, "x 0:01:00.0 y");
    n = nvgpu_pcimap_text(&sm, buf, strlen(buf));
    buf[n] = '\0';
    CHECK(strcmp(buf, "x 0:01:00.0 y") == 0);
  }

  /* Text with NULs or nothing in it is left alone. */
  memcpy(buf, "x\0y", 3);
  CHECK(nvgpu_pcimap_text(&m, buf, 3) == 3 && memcmp(buf, "x\0y", 3) == 0);
  CHECK(nvgpu_pcimap_text(&m, buf, 0) == 0);
}

static void put32(u8 *p, u32 v) { nvgpu_pcimap_wr32(p, v); }
static u32 get32(const u8 *p) { return nvgpu_pcimap_rd32(p); }

static struct nvgpu_pcimap one_gpu(void) {
  static const char *const addrs[] = {"0000:01:00.0"};
  static const long guest[] = {0, -1};
  struct nvgpu_pcimap m;

  build(&m, addrs, 1, guest);
  return m;
}

static void test_card_info(void) {
  struct nvgpu_pcimap m = one_gpu();
  u8 buf[NVGPU_CARD_INFO_SIZE * 3];

  memset(buf, 0, sizeof(buf));
  /* Entry 0: the GPU. Entry 1: invalid, domain 0. Entry 2: valid, domain 5. */
  buf[0] = 1;
  put32(buf + 4, 0);
  buf[8] = 1;
  put32(buf + 16, 0x100); /* gpu_id stays the host's */
  put32(buf + NVGPU_CARD_INFO_SIZE + 4, 0);
  buf[2 * NVGPU_CARD_INFO_SIZE] = 1;
  put32(buf + 2 * NVGPU_CARD_INFO_SIZE + 4, 5);

  CHECK(nvgpu_pcimap_card_info(&m, buf, sizeof(buf)) == 1);
  CHECK(get32(buf + 4) == 0x10 && buf[8] == 1);
  CHECK(get32(buf + 16) == 0x100);
  CHECK(get32(buf + NVGPU_CARD_INFO_SIZE + 4) == 0);
  CHECK(get32(buf + 2 * NVGPU_CARD_INFO_SIZE + 4) == 5);
  /* A short trailing entry is not read. */
  CHECK(nvgpu_pcimap_card_info(&m, buf, NVGPU_CARD_INFO_SIZE - 1) == 0);
}

static void test_status_code(void) {
  struct nvgpu_pcimap m = one_gpu();
  struct nvgpu_pcimap_saved s;
  u8 buf[NVGPU_STATUS_CODE_SIZE] = {0};

  put32(buf, 0x10);
  buf[4] = 1;
  CHECK(nvgpu_pcimap_status_code_in(&m, buf, sizeof(buf), &s));
  CHECK(get32(buf) == 0 && s.at == 0);
  put32(buf + 8, 0x1234); /* RM's answer */
  nvgpu_pcimap_restore(&s, buf, sizeof(buf));
  CHECK(get32(buf) == 0x10 && get32(buf + 8) == 0x1234);

  /* An address in a domain that is not a mirror is sent as it is. */
  put32(buf, 0);
  CHECK(!nvgpu_pcimap_status_code_in(&m, buf, sizeof(buf), &s));
  CHECK(get32(buf) == 0 && s.at == -1);
  nvgpu_pcimap_restore(&s, buf, sizeof(buf));
  CHECK(get32(buf) == 0);
  CHECK(!nvgpu_pcimap_status_code_in(&m, buf, 8, &s));
}

static void test_rmctrl(void) {
  struct nvgpu_pcimap m = one_gpu();
  struct nvgpu_pcimap_saved s;
  u8 p[4 + 52 * 8];

  /* GET_PCI_INFO {gpuId, domain, bus16, slot16} */
  memset(p, 0, sizeof(p));
  put32(p, 0x100);
  put32(p + 4, 0);
  p[8] = 1;
  CHECK(nvgpu_pcimap_rmctrl_out(&m, NVGPU_RM_GPU_GET_PCI_INFO, p, 12) == 1);
  CHECK(get32(p) == 0x100 && get32(p + 4) == 0x10 && p[8] == 1);
  put32(p + 4, 0);
  CHECK(nvgpu_pcimap_rmctrl_out(&m, NVGPU_RM_GPU_GET_PCI_INFO, p, 16) == 0);
  CHECK(!nvgpu_pcimap_rmctrl_in(&m, NVGPU_RM_GPU_GET_PCI_INFO, p, 12, &s));

  /* BUS_GET_INFO_V2: only the DOMAIN_NUMBER entry, only within the count. */
  memset(p, 0, sizeof(p));
  put32(p, 3);
  put32(p + 4, 0x0f); /* BUS_NUMBER */
  put32(p + 8, 1);
  put32(p + 12, 0x2c); /* DOMAIN_NUMBER */
  put32(p + 16, 0);
  put32(p + 20, 0x10); /* DEVICE_NUMBER */
  put32(p + 24, 0);
  put32(p + 28, 0x2c); /* past the count */
  put32(p + 32, 0);
  CHECK(nvgpu_pcimap_rmctrl_out(&m, NVGPU_RM_BUS_GET_INFO_V2, p, sizeof(p)) ==
        1);
  CHECK(get32(p + 8) == 1 && get32(p + 16) == 0x10 && get32(p + 24) == 0);
  CHECK(get32(p + 32) == 0);
  /* A count larger than the block is bounded by the block. */
  put32(p, 0xffffffff);
  put32(p + 16, 0);
  CHECK(nvgpu_pcimap_rmctrl_out(&m, NVGPU_RM_BUS_GET_INFO_V2, p, 20) == 1);

  /* The V1 list, as the pointer's segment returns it. */
  memset(p, 0, 16);
  put32(p, 0x2c);
  put32(p + 8, 0x2c);
  put32(p + 12, 7);
  CHECK(nvgpu_pcimap_bus_info_list(&m, p, 16, 2, true) == 1);
  CHECK(get32(p + 4) == 0x10 && get32(p + 12) == 7);

  /* PCIE atomics: dbdf in, restored after. */
  memset(p, 0, 12);
  put32(p, 2);
  put32(p + 4, 0x00100100 | (0 << 3)); /* 0010:01:00.0 */
  CHECK(nvgpu_pcimap_rmctrl_in(&m, NVGPU_RM_BUS_PCIE_REQ_ATOMICS_CAPS, p, 12,
                               &s));
  CHECK(get32(p + 4) == 0x00000100);
  put32(p + 8, 0xab);
  CHECK(nvgpu_pcimap_rmctrl_out(&m, NVGPU_RM_BUS_PCIE_REQ_ATOMICS_CAPS, p,
                                12) == 0);
  nvgpu_pcimap_restore(&s, p, 12);
  CHECK(get32(p + 4) == 0x00100100 && get32(p + 8) == 0xab);

  put32(p + 4, 0x00000100); /* not a mirror's domain: untouched */
  CHECK(!nvgpu_pcimap_rmctrl_in(&m, NVGPU_RM_BUS_PCIE_SUPPORTED_GPU_ATOMICS, p,
                                12, &s));
  CHECK(get32(p + 4) == 0x00000100);

  /* Anything else is left alone. */
  memset(p, 0, 12);
  CHECK(nvgpu_pcimap_rmctrl_out(&m, 0x20801801, p, 12) == 0);
  CHECK(get32(p + 4) == 0);
}

int main(void) {
  test_parse();
  test_build_q35();
  test_build_skips_used();
  test_build_multi();
  test_text();
  test_card_info();
  test_status_code();
  test_rmctrl();
  if (failures) {
    fprintf(stderr, "pcimap_test: %d failure(s)\n", failures);
    return EXIT_FAILURE;
  }
  printf("pcimap_test: ok\n");
  return EXIT_SUCCESS;
}
