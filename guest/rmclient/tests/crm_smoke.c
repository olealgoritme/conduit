/* SPDX-License-Identifier: MIT */
/*
 * crm_smoke: librmclient against a real RM (inside a Conduit guest, or on a
 * machine with NVIDIA's driver). Prints every step; exits non-zero if a
 * required step failed. Frees everything it allocated.
 */
#include <errno.h>
#include <inttypes.h>
#include <stdio.h>
#include <string.h>

#include "rmclient.h"
#include "nv_ioctl_defs.h"

#define MIB (1024ull * 1024ull)

static int failed;

static int step(const char *what, int r)
{
    if (r == 0)
        printf("[ ok ] %s\n", what);
    else
        printf("[FAIL] %s: %s (%d / 0x%x) %s\n", what, crm_status_name(r), r,
               (unsigned)r, crm_status_string(r));
    fflush(stdout);
    if (r)
        failed = 1;
    return r;
}

static const char *arch_name(uint32_t a)
{
    switch (a) {
    /* NV2080_CTRL_MC_ARCHITECTURE_*: one value per family, GeForce parts included. */
    case 0x160: return "Turing (TU10x, RTX 20)";
    case 0x170: return "Ampere (GA10x, RTX 30)";
    case 0x190: return "Ada (AD10x, RTX 40)";
    case 0x180: return "Hopper (GH100)";
    case 0x1A0: return "Blackwell (GB10x)";
    case 0x1B0: return "Blackwell (GB20x, RTX 50)";
    case 0x1C0: return "Rubin (GR100)";
    default:    return "?";
    }
}

int main(void)
{
    crm_client *c = NULL;
    int r = crm_open(&c, NULL);
    if (step("crm_open (NV_ESC_CHECK_VERSION_STR, NV_ESC_CARD_INFO, NV01_ROOT_CLIENT)", r))
        return 1;
    printf("       RM version %s, root client 0x%08x\n", crm_rm_version(c), crm_root(c));

    int n = crm_gpu_count(c);
    printf("[%s] CARD_INFO: %d GPU(s)\n", n > 0 ? " ok " : "FAIL", n);
    for (int i = 0; i < n; i++) {
        uint32_t id, minor, devid, domain;
        uint8_t bus, slot, fn;
        uint16_t vendor;
        crm_gpu_info(c, i, &id, &minor, &devid);
        crm_gpu_pci(c, i, &domain, &bus, &slot, &fn, &vendor);
        printf("       gpu %d: gpu_id 0x%x minor %u pci %04x:%02x:%02x.%x %04x:%04x\n",
               i, id, minor, domain, bus, slot, fn, vendor, devid);
    }
    if (n <= 0) {
        crm_close(c);
        return 1;
    }

    uint32_t dev = 0, sub = 0, vas = 0, vid = 0, sys = 0;
    void *sys_cpu = NULL, *vid_cpu = NULL;
    uint64_t vid_va = 0, sys_va = 0;

    NV0080_ALLOC_PARAMETERS dp;
    memset(&dp, 0, sizeof(dp));
    dp.deviceId = 0;
    if (step("alloc NV01_DEVICE_0", crm_alloc(c, crm_root(c), &dev, NV01_DEVICE_0, &dp, sizeof(dp))))
        goto out;
    printf("       device 0x%08x\n", dev);

    NV2080_ALLOC_PARAMETERS sp = { .subDeviceId = 0 };
    if (step("alloc NV20_SUBDEVICE_0", crm_alloc(c, dev, &sub, NV20_SUBDEVICE_0, &sp, sizeof(sp))))
        goto out;
    printf("       subdevice 0x%08x\n", sub);

    NV2080_CTRL_GPU_GET_NAME_STRING_PARAMS name;
    memset(&name, 0, sizeof(name));
    name.gpuNameStringFlags = NV2080_CTRL_GPU_GET_NAME_STRING_FLAGS_TYPE_ASCII;
    if (!step("control NV2080_CTRL_CMD_GPU_GET_NAME_STRING",
              crm_control(c, sub, NV2080_CTRL_CMD_GPU_GET_NAME_STRING, &name, sizeof(name)))) {
        name.gpuNameString.ascii[sizeof(name.gpuNameString.ascii) - 1] = 0;
        printf("       name \"%s\"\n", (char *)name.gpuNameString.ascii);
    }

    NV2080_CTRL_MC_GET_ARCH_INFO_PARAMS arch;
    memset(&arch, 0, sizeof(arch));
    if (!step("control NV2080_CTRL_CMD_MC_GET_ARCH_INFO",
              crm_control(c, sub, NV2080_CTRL_CMD_MC_GET_ARCH_INFO, &arch, sizeof(arch))))
        printf("       architecture 0x%x %s, implementation 0x%x, revision 0x%x\n",
               arch.architecture, arch_name(arch.architecture), arch.implementation, arch.revision);

    NV_VASPACE_ALLOCATION_PARAMETERS vp;
    memset(&vp, 0, sizeof(vp));
    if (!step("alloc FERMI_VASPACE_A", crm_alloc(c, dev, &vas, FERMI_VASPACE_A, &vp, sizeof(vp))))
        printf("       vaspace 0x%08x, size 0x%" PRIx64 ", base 0x%" PRIx64 ", big page %u\n",
               vas, vp.vaSize, vp.vaBase, vp.bigPageSize);

    NV_MEMORY_ALLOCATION_PARAMS mp;
    memset(&mp, 0, sizeof(mp));
    mp.owner = crm_root(c);
    mp.type = NVOS32_TYPE_IMAGE;
    mp.flags = NVOS32_ALLOC_FLAGS_ALIGNMENT_FORCE | NVOS32_ALLOC_FLAGS_IGNORE_BANK_PLACEMENT;
    mp.attr = (NVOS32_ATTR_LOCATION_VIDMEM << NVOS32_ATTR_LOCATION_SHIFT) |
              (NVOS32_ATTR_PAGE_SIZE_HUGE << NVOS32_ATTR_PAGE_SIZE_SHIFT) |
              (NVOS32_ATTR_PHYSICALITY_CONTIGUOUS << NVOS32_ATTR_PHYSICALITY_SHIFT);
    mp.size = 2 * MIB;
    mp.alignment = 2 * MIB;
    if (!step("alloc NV01_MEMORY_LOCAL_USER 2 MiB", crm_alloc(c, dev, &vid, NV01_MEMORY_LOCAL_USER, &mp, sizeof(mp))))
        printf("       vidmem 0x%08x, size 0x%" PRIx64 ", offset 0x%" PRIx64 ", attr 0x%08x\n",
               vid, mp.size, mp.offset, mp.attr);

    memset(&mp, 0, sizeof(mp));
    mp.owner = crm_root(c);
    mp.type = NVOS32_TYPE_IMAGE;
    mp.attr = (NVOS32_ATTR_LOCATION_PCI << NVOS32_ATTR_LOCATION_SHIFT) |
              (NVOS32_ATTR_PAGE_SIZE_4KB << NVOS32_ATTR_PAGE_SIZE_SHIFT) |
              (NVOS32_ATTR_PHYSICALITY_NONCONTIGUOUS << NVOS32_ATTR_PHYSICALITY_SHIFT) |
              (NVOS32_ATTR_COHERENCY_CACHED << NVOS32_ATTR_COHERENCY_SHIFT);
    mp.size = 2 * MIB;
    if (!step("alloc NV01_MEMORY_SYSTEM 2 MiB", crm_alloc(c, dev, &sys, NV01_MEMORY_SYSTEM, &mp, sizeof(mp))))
        printf("       sysmem 0x%08x, size 0x%" PRIx64 ", attr 0x%08x\n", sys, mp.size, mp.attr);

    if (sys && !step("CPU-map system memory", crm_map_memory(c, dev, sys, 0, 2 * MIB, 0, &sys_cpu))) {
        volatile uint32_t *w = sys_cpu;
        size_t words = 2 * MIB / 4, bad = 0;
        for (size_t i = 0; i < words; i++)
            w[i] = (uint32_t)(i * 2654435761u) ^ 0x5a5a5a5au;
        for (size_t i = 0; i < words; i++)
            bad += w[i] != ((uint32_t)(i * 2654435761u) ^ 0x5a5a5a5au);
        printf("       mapped at %p\n", sys_cpu);
        step("write/read 2 MiB through the CPU mapping", bad ? -EIO : 0);
        if (bad)
            printf("       %zu mismatching words\n", bad);
    }

    /* Not in the required list: BAR1 CPU mapping of video memory. */
    if (vid) {
        r = crm_map_memory(c, dev, vid, 0, 64 * 1024, 0, &vid_cpu);
        if (r == 0) {
            volatile uint32_t *w = vid_cpu;
            w[0] = 0xc0ffee11u;
            w[4095] = 0x12345678u;
            int okv = w[0] == 0xc0ffee11u && w[4095] == 0x12345678u;
            printf("[ ok ] (extra) CPU-map 64 KiB of video memory at %p, write/read %s\n",
                   vid_cpu, okv ? "matches" : "MISMATCH");
        } else {
            printf("[info] (extra) CPU-map of video memory: %s (0x%x) %s\n",
                   crm_status_name(r), (unsigned)r, crm_status_string(r));
        }
    }

    /* Extra: the usermode (doorbell) object lives under the subdevice and is
     * CPU-mapped with the subdevice handle as `device`. */
    {
        static const uint32_t um_classes[] = { 0xc761 /* BLACKWELL */, 0xc661 /* HOPPER */,
                                               0xc561 /* AMPERE */, 0xc461 /* TURING */ };
        uint32_t um = 0, um_class = 0;
        for (unsigned i = 0; i < sizeof(um_classes) / sizeof(um_classes[0]) && !um; i++) {
            uint32_t h = 0;
            if (crm_alloc(c, sub, &h, um_classes[i], NULL, 0) == 0) {
                um = h;
                um_class = um_classes[i];
            }
        }
        if (!um) {
            printf("[info] (extra) no usermode class could be allocated\n");
        } else {
            void *ump = NULL;
            r = crm_map_memory(c, sub, um, 0, 0x10000, 0, &ump);
            if (r == 0) {
                volatile uint32_t *w = ump;
                printf("[ ok ] (extra) usermode 0x%x 0x%08x CPU-mapped via the subdevice at %p, word[0x10]=0x%08x\n",
                       um_class, um, ump, w[0x10 / 4]);
                step("(extra) CPU-unmap usermode", crm_unmap_memory(c, sub, um, ump, 0x10000, 0));
            } else {
                printf("[info] (extra) CPU-map of usermode 0x%x via the subdevice: %s (0x%x) %s\n",
                       um_class, crm_status_name(r), (unsigned)r, crm_status_string(r));
            }
            step("(extra) free usermode", crm_free(c, sub, um));
        }
    }

    /* Extra: an OS event on the subdevice (NV2080_NOTIFIERS_SW = 0). */
    {
        uint32_t ev = 0;
        int efd = -1;
        r = crm_event_open(c, sub, 0, &ev, &efd);
        if (r == 0) {
            struct crm_event_data d[4];
            int nd = crm_event_drain(c, efd, d, 4);
            printf("[ ok ] (extra) OS event 0x%08x on fd %d, %d pending\n", ev, efd, nd);
            step("(extra) close OS event", crm_event_close(c, ev, efd));
        } else {
            printf("[info] (extra) OS event: %s (0x%x) %s\n", crm_status_name(r), (unsigned)r,
                   crm_status_string(r));
        }
    }

    if (vas && vid && !step("GPU-map video memory into the VA space",
                            crm_map_dma(c, dev, vas, vid, 0, 2 * MIB, 0, &vid_va)))
        printf("       vidmem GPU VA 0x%" PRIx64 "\n", vid_va);
    if (vas && sys && !step("GPU-map system memory into the VA space",
                            crm_map_dma(c, dev, vas, sys, 0, 2 * MIB, 0, &sys_va)))
        printf("       sysmem GPU VA 0x%" PRIx64 "\n", sys_va);

    if (vid_va)
        step("GPU-unmap video memory", crm_unmap_dma(c, dev, vas, vid, 0, vid_va));
    if (sys_va)
        step("GPU-unmap system memory", crm_unmap_dma(c, dev, vas, sys, 0, sys_va));
    if (vid_cpu)
        step("(extra) CPU-unmap video memory", crm_unmap_memory(c, dev, vid, vid_cpu, 64 * 1024, 0));
    if (sys_cpu)
        step("CPU-unmap system memory", crm_unmap_memory(c, dev, sys, sys_cpu, 2 * MIB, 0));

out:
    if (sys) step("free system memory", crm_free(c, dev, sys));
    if (vid) step("free video memory", crm_free(c, dev, vid));
    if (vas) step("free VA space", crm_free(c, dev, vas));
    if (sub) step("free subdevice", crm_free(c, dev, sub));
    if (dev) step("free device", crm_free(c, crm_root(c), dev));
    printf("       objects still tracked: %zu, CPU mappings: %zu\n",
           crm_object_count(c), crm_mapping_count(c));
    crm_close(c);
    printf("[ ok ] crm_close\n");
    printf("%s\n", failed ? "SMOKE FAILED" : "SMOKE PASSED");
    return failed ? 1 : 0;
}
