/* OS-descriptor (PIN) smoke: crm_alloc_pages -> crm_alloc_os_descriptor ->
 * GPU map/unmap -> free, at 2 MiB and 512 MiB (indirect page-run table). */
#include <inttypes.h>
#include <stdio.h>
#include <string.h>
#include "rmclient.h"
#include "nv_ioctl_defs.h"
#define MIB (1024ull * 1024ull)
static int fails;
static int step(const char *what, int r)
{
    if (r) { printf("[FAIL] %s: %s (%d / 0x%x)\n", what, crm_status_name(r), r, (unsigned)r); fails++; }
    else printf("[ ok ] %s\n", what);
    return r;
}
static void one(crm_client *c, uint32_t dev, uint32_t vas, uint64_t size)
{
    char w[128];
    void *p = NULL;
    snprintf(w, sizeof w, "alloc_pages %" PRIu64 " MiB", (uint64_t)(size / MIB));
    if (step(w, crm_alloc_pages(c, size, &p))) return;
    for (uint64_t i = 0; i < size; i += 4096) ((volatile uint32_t *)p)[i / 4] = (uint32_t)(i >> 12);
    uint32_t obj = 0;
    snprintf(w, sizeof w, "os_descriptor %" PRIu64 " MiB", (uint64_t)(size / MIB));
    if (!step(w, crm_alloc_os_descriptor(c, dev, &obj, p, size, 0))) {
        printf("       object 0x%08x at %p\n", obj, p);
        uint64_t va = 0;
        if (!step("  GPU-map os descriptor", crm_map_dma(c, dev, vas, obj, 0, size, 0, &va))) {
            printf("       GPU VA 0x%" PRIx64 "\n", va);
            step("  GPU-unmap os descriptor", crm_unmap_dma(c, dev, vas, obj, 0, va));
        }
        int ok = 1;
        for (uint64_t i = 0; i < size; i += 4096) if (((volatile uint32_t *)p)[i / 4] != (uint32_t)(i >> 12)) { ok = 0; break; }
        step("  CPU pages intact", ok ? 0 : -5);
        step("  free os descriptor", crm_free(c, dev, obj));
    }
    step("  free_pages", crm_free_pages(c, p, size));
}
int main(void)
{
    crm_client *c = NULL;
    if (step("crm_open", crm_open(&c, NULL))) return 1;
    uint32_t dev = 0, vas = 0;
    NV0080_ALLOC_PARAMETERS dp; memset(&dp, 0, sizeof dp);
    if (step("alloc NV01_DEVICE_0", crm_alloc(c, crm_root(c), &dev, NV01_DEVICE_0, &dp, sizeof dp))) goto out;
    NV_VASPACE_ALLOCATION_PARAMETERS vp; memset(&vp, 0, sizeof vp);
    if (step("alloc FERMI_VASPACE_A", crm_alloc(c, dev, &vas, FERMI_VASPACE_A, &vp, sizeof vp))) goto out;
    one(c, dev, vas, 2 * MIB);
    one(c, dev, vas, 512 * MIB);
    step("free VA space", crm_free(c, dev, vas));
    step("free device", crm_free(c, crm_root(c), dev));
out:
    printf("       objects still tracked: %zu, CPU mappings: %zu\n", crm_object_count(c), crm_mapping_count(c));
    crm_close(c);
    printf(fails ? "PIN SMOKE FAILED\n" : "PIN SMOKE PASSED\n");
    return fails != 0;
}
