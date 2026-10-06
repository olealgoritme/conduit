/* SPDX-License-Identifier: MIT */
/*
 * host_import_spike: can NVIDIA's proprietary Vulkan driver on the host import
 * memory that NVK on RM allocated through RM, exported as a dma-buf through
 * nvidia-drm, as a VkImage with the right layout?  (Linux host only.)
 *
 * For each layout under test (DRM_FORMAT_MOD_LINEAR and NVIDIA block-linear
 * modifiers):
 *
 *   1. RM side, exactly as nvk-rm allocates and exports images (patches 0004,
 *      0013): NV01_MEMORY_LOCAL_USER, VIDMEM, 64 KiB pages, non-contiguous,
 *      no compression, no kind on the allocation (NVK applies kinds per GPU
 *      mapping); CPU map; write a test pattern in the layout under test
 *      (block-linear through a CPU GOB swizzle, TuringColor2D as NIL
 *      defines it); NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD into a fresh
 *      /dev/nvidiactl fd; DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY on the host's
 *      nvidia-drm render node with the NVKMS surface params nvk-rm passes;
 *      PRIME handle -> dma-buf fd.
 *   2. Vulkan side (NVIDIA ICD): VK_EXT_external_memory_dma_buf +
 *      VK_EXT_image_drm_format_modifier, VkImage with an explicit modifier
 *      and plane layout, import the dma-buf, bind.
 *   3. Read check: copy the whole image to a buffer; every pixel must be the
 *      pattern value for its coordinate.  The pattern encodes the coordinate
 *      ((y << 16) | x), so a mismatch says where the pixel came from.
 *   4. Write check: copy a known block into a sub-rectangle that crosses GOB
 *      and block boundaries, then read the RM CPU mapping back (deswizzling
 *      on the CPU) and check where the pixels landed and that nothing else
 *      changed.
 *
 * Plus: the modifiers NVIDIA advertises for B8G8R8A8, plane-layout
 * alignment probes, and the "opaque" routes (OPTIMAL tiling and/or
 * OPAQUE_FD handle type with the dma-buf or with the raw RM export fd).
 *
 * Result on an RTX 5090 (GB202), driver 610.57.04: NVIDIA advertises
 * 0x0300000000606010..15 (BL2D, kind 0x06, gob kind generation 2, sector
 * layout 1, uncompressed, h = 0..5) and LINEAR for B8G8R8A8/R8G8B8A8, the
 * same list NIL builds.  LINEAR (rowPitch 7680), BL h=5 (0x0300000000606015,
 * what NVK picks for a 1920x1080 swapchain image), h=4 and h=0 all import,
 * read back pixel-exact and write to the expected RM bytes.  No dedicated
 * allocation needed; image size may be smaller than the dma-buf (RM rounds
 * to 64 KiB).  OPTIMAL tiling fails to import either fd (dma-buf as DMA_BUF
 * or OPAQUE_FD, RM export fd as OPAQUE_FD) with VK_ERROR_OUT_OF_DEVICE_MEMORY;
 * NVIDIA's own OPTIMAL 1920x1080 is 0x870000 B, i.e. h=4, not NVK's h=5.
 *
 * Build (from guest/rmclient, after `make`):
 *   cc -std=gnu11 -O2 -g -Wall -Iinclude -Isrc $(pkg-config --cflags libdrm) \
 *      ../nvk-rm/tests/host_import_spike.c build-make/librmclient.a \
 *      -lvulkan -ldrm -pthread -o build-make/host_import_spike
 * Run:
 *   VK_DRIVER_FILES=/usr/share/vulkan/icd.d/nvidia_icd.json \
 *      build-make/host_import_spike [/dev/dri/renderD128]
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <stdarg.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

#include <xf86drm.h>
#include <drm_fourcc.h>
#include <vulkan/vulkan.h>

#include "rmclient.h"
#include "nv_ioctl_defs.h"

#define W 1920u
#define H 1080u
#define BPP 4u

/* ---- nvidia-drm / NVKMS (nvidia-drm-ioctl.h, nvkms-kapi-private.h) ------ */

struct drm_nvidia_get_dev_info_params {
    uint32_t gpu_id, mig_device, primary_index, supports_alloc;
    uint32_t generic_page_kind, page_kind_generation, sector_layout;
    uint32_t supports_sync_fd, supports_semsurf;
};
struct drm_nvidia_gem_import_nvkms_memory_params {
    uint64_t mem_size;
    uint64_t nvkms_params_ptr;
    uint64_t nvkms_params_size;
    uint32_t handle;
    uint32_t pad;
};
struct nvkms_kapi_priv_import_memory_params {
    int32_t memFd;
    uint32_t layout; /* 0 block linear, 1 pitch */
    uint32_t log2_gobs_x, log2_gobs_y, log2_gobs_z;
    uint32_t pitch_in_blocks;
    uint8_t generic_memory;
    uint8_t pad[3];
};
_Static_assert(sizeof(struct drm_nvidia_get_dev_info_params) == 36, "GET_DEV_INFO");
_Static_assert(sizeof(struct drm_nvidia_gem_import_nvkms_memory_params) == 32, "GEM_IMPORT");
_Static_assert(sizeof(struct nvkms_kapi_priv_import_memory_params) == 28, "NVKMS import");

#define DRM_IOCTL_NVIDIA_GET_DEV_INFO \
    DRM_IOWR(DRM_COMMAND_BASE + 0x03, struct drm_nvidia_get_dev_info_params)
#define DRM_IOCTL_NVIDIA_GEM_IMPORT_NVKMS_MEMORY \
    DRM_IOWR(DRM_COMMAND_BASE + 0x01, struct drm_nvidia_gem_import_nvkms_memory_params)

#define NVOS32_ATTR_PHYSICALITY_ALLOW_NONCONTIGUOUS 0x3u
#define NVOS32_ATTR2_ZBC_PREFER_NO_ZBC 0x2u

/* ---- helpers -------------------------------------------------------------- */

static int failures;

static void report(bool ok, const char *fmt, ...) __attribute__((format(printf, 2, 3)));
static void report(bool ok, const char *fmt, ...)
{
    va_list ap;
    va_start(ap, fmt);
    printf("[%s] ", ok ? " ok " : "FAIL");
    vprintf(fmt, ap);
    printf("\n");
    va_end(ap);
    fflush(stdout);
    if (!ok)
        failures++;
}

static const char *vkr(VkResult r)
{
    switch (r) {
    case VK_SUCCESS: return "VK_SUCCESS";
    case VK_ERROR_OUT_OF_HOST_MEMORY: return "VK_ERROR_OUT_OF_HOST_MEMORY";
    case VK_ERROR_OUT_OF_DEVICE_MEMORY: return "VK_ERROR_OUT_OF_DEVICE_MEMORY";
    case VK_ERROR_INITIALIZATION_FAILED: return "VK_ERROR_INITIALIZATION_FAILED";
    case VK_ERROR_FORMAT_NOT_SUPPORTED: return "VK_ERROR_FORMAT_NOT_SUPPORTED";
    case VK_ERROR_FEATURE_NOT_PRESENT: return "VK_ERROR_FEATURE_NOT_PRESENT";
    case VK_ERROR_EXTENSION_NOT_PRESENT: return "VK_ERROR_EXTENSION_NOT_PRESENT";
    case VK_ERROR_INVALID_EXTERNAL_HANDLE: return "VK_ERROR_INVALID_EXTERNAL_HANDLE";
    case VK_ERROR_INVALID_DRM_FORMAT_MODIFIER_PLANE_LAYOUT_EXT:
        return "VK_ERROR_INVALID_DRM_FORMAT_MODIFIER_PLANE_LAYOUT_EXT";
    case VK_ERROR_UNKNOWN: return "VK_ERROR_UNKNOWN";
    default: {
        static char b[32];
        snprintf(b, sizeof b, "VkResult %d", r);
        return b;
    }
    }
}

/* Coordinate-encoding pattern: a pixel's value names the pixel. */
static inline uint32_t pat(uint32_t x, uint32_t y) { return (y << 16) | x; }
/* Write-check pattern: distinguishable from pat() by the top byte. */
static inline uint32_t pat2(uint32_t x, uint32_t y) { return 0xc0000000u | (y << 12) | x; }

/* ---- layouts -------------------------------------------------------------- */

struct layout {
    const char *name;
    uint64_t modifier;
    bool block_linear;
    uint32_t log2_gob_h; /* block height in GOBs, log2 */
    uint32_t pitch;      /* bytes per row of pixels (linear) / of GOBs*64 (BL) */
    uint64_t size;       /* bytes the layout occupies */
};

/* NIL's TuringColor2D GOB (64 B x 8 rows, 16 B x 2 row sectors; see
 * src/nouveau/nil/copy.rs CopyGOBTuring2D). Byte x (0..63), row y (0..7). */
static inline uint32_t gob_off(uint32_t xb, uint32_t y)
{
    return (xb / 32) * 256 + (y / 4) * 128 + ((xb % 32) / 16) * 64 + (y % 4) * 16 + (xb % 16);
}

/* Offset of pixel (x, y) in the layout. Block-linear 2D: blocks are 1 GOB
 * wide and 2^h GOBs tall, laid out row-major; GOBs inside a block stacked
 * vertically. */
static inline uint64_t px_off(const struct layout *l, uint32_t x, uint32_t y)
{
    if (!l->block_linear)
        return (uint64_t)y * l->pitch + (uint64_t)x * BPP;
    const uint32_t xb = x * BPP;
    const uint32_t gobs_x = l->pitch / 64;
    const uint32_t bh = 8u << l->log2_gob_h; /* rows per block */
    const uint64_t block = (uint64_t)(y / bh) * gobs_x + xb / 64;
    const uint32_t gob_in_block = (y % bh) / 8;
    return block * (512u << l->log2_gob_h) + gob_in_block * 512u + gob_off(xb % 64, y % 8);
}

static void layout_init(struct layout *l, const char *name, uint64_t mod)
{
    memset(l, 0, sizeof *l);
    l->name = name;
    l->modifier = mod;
    if (mod == DRM_FORMAT_MOD_LINEAR) {
        l->pitch = W * BPP; /* 7680, 256-aligned */
        l->size = (uint64_t)l->pitch * H;
    } else {
        l->block_linear = true;
        l->log2_gob_h = (uint32_t)(mod & 0xf);
        l->pitch = (W * BPP + 63) & ~63u;
        const uint32_t bh = 8u << l->log2_gob_h;
        const uint32_t brows = (H + bh - 1) / bh;
        l->size = (uint64_t)(l->pitch / 64) * brows * (512u << l->log2_gob_h);
    }
}

/* ---- RM side -------------------------------------------------------------- */

struct rm {
    crm_client *c;
    uint32_t dev, sub;
    int drm_fd;
};

struct surf {
    uint32_t mem;
    uint64_t size;
    volatile uint8_t *cpu;
    uint32_t gem;
    int dmabuf_fd;
    int rm_export_fd; /* a second RM export (for the opaque-fd probe) */
};

static int rm_alloc(struct rm *rm, uint64_t want, struct surf *s)
{
    memset(s, 0, sizeof *s);
    s->dmabuf_fd = s->rm_export_fd = -1;
    NV_MEMORY_ALLOCATION_PARAMS mp;
    memset(&mp, 0, sizeof mp);
    mp.owner = crm_root(rm->c);
    mp.type = NVOS32_TYPE_IMAGE;
    mp.flags = NVOS32_ALLOC_FLAGS_ALIGNMENT_FORCE;
    /* nvk-rm's VRAM allocation (patch 0004): no width/height/format/kind. */
    mp.attr = (NVOS32_ATTR_LOCATION_VIDMEM << NVOS32_ATTR_LOCATION_SHIFT) |
              (NVOS32_ATTR_PAGE_SIZE_BIG << NVOS32_ATTR_PAGE_SIZE_SHIFT) |
              (NVOS32_ATTR_PHYSICALITY_ALLOW_NONCONTIGUOUS << NVOS32_ATTR_PHYSICALITY_SHIFT);
    mp.attr2 = NVOS32_ATTR2_ZBC_PREFER_NO_ZBC |
               (NVOS32_ATTR2_GPU_CACHEABLE_YES << NVOS32_ATTR2_GPU_CACHEABLE_SHIFT);
    mp.size = (want + 0xffff) & ~0xffffull;
    mp.alignment = 64 * 1024;
    int r = crm_alloc(rm->c, rm->dev, &s->mem, NV01_MEMORY_LOCAL_USER, &mp, sizeof mp);
    report(r == 0, "  RM alloc NV01_MEMORY_LOCAL_USER 0x%" PRIx64 " B -> 0x%08x (%s)",
           (uint64_t)((want + 0xffff) & ~0xffffull), s->mem, crm_status_name(r));
    if (r)
        return r;
    s->size = mp.size;
    void *p = NULL;
    r = crm_map_memory(rm->c, rm->dev, s->mem, 0, s->size, 0, &p);
    report(r == 0, "  RM CPU map (%s)", crm_status_name(r));
    if (r)
        return r;
    s->cpu = p;
    return 0;
}

static int rm_export_fd(struct rm *rm, struct surf *s, int *out)
{
    int ctl = open("/dev/nvidiactl", O_RDWR | O_CLOEXEC);
    if (ctl < 0)
        return -errno;
    NV0000_CTRL_OS_UNIX_EXPORT_OBJECT_TO_FD_PARAMS ex;
    memset(&ex, 0, sizeof ex);
    ex.object.type = NV0000_CTRL_OS_UNIX_EXPORT_OBJECT_TYPE_RM;
    ex.object.data.rmObject.hDevice = rm->dev;
    ex.object.data.rmObject.hParent = rm->dev;
    ex.object.data.rmObject.hObject = s->mem;
    ex.fd = ctl;
    int r = crm_control(rm->c, crm_root(rm->c), NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD, &ex,
                        sizeof ex);
    if (r) {
        close(ctl);
        return r;
    }
    *out = ctl;
    return 0;
}

static int rm_export(struct rm *rm, const struct layout *l, struct surf *s)
{
    int ctl = -1;
    int r = rm_export_fd(rm, s, &ctl);
    report(r == 0, "  NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD (%s)", crm_status_name(r));
    if (r)
        return r;

    /* nvk-rm's surface params (patch 0013 export_to_gem). */
    struct nvkms_kapi_priv_import_memory_params nv;
    memset(&nv, 0, sizeof nv);
    nv.memFd = ctl;
    nv.layout = l->block_linear ? 0 : 1;
    nv.log2_gobs_y = l->block_linear ? l->log2_gob_h : 0;
    nv.generic_memory = l->block_linear;
    struct drm_nvidia_gem_import_nvkms_memory_params imp;
    memset(&imp, 0, sizeof imp);
    imp.mem_size = s->size;
    imp.nvkms_params_ptr = (uint64_t)(uintptr_t)&nv;
    imp.nvkms_params_size = sizeof nv;
    r = drmIoctl(rm->drm_fd, DRM_IOCTL_NVIDIA_GEM_IMPORT_NVKMS_MEMORY, &imp) ? -errno : 0;
    close(ctl);
    report(r == 0 && imp.handle, "  DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY -> GEM %u (%s)", imp.handle,
           r ? strerror(-r) : "ok");
    if (r)
        return r;
    s->gem = imp.handle;

    r = drmPrimeHandleToFD(rm->drm_fd, s->gem, DRM_CLOEXEC | DRM_RDWR, &s->dmabuf_fd);
    if (r)
        r = -errno;
    off_t dsz = r ? -1 : lseek(s->dmabuf_fd, 0, SEEK_END);
    report(r == 0, "  PRIME handle -> dma-buf fd %d (size 0x%llx)", s->dmabuf_fd,
           (unsigned long long)dsz);
    if (r)
        return r;

    r = rm_export_fd(rm, s, &s->rm_export_fd);
    if (r)
        s->rm_export_fd = -1;
    return 0;
}

static void rm_free(struct rm *rm, struct surf *s)
{
    if (s->dmabuf_fd >= 0)
        close(s->dmabuf_fd);
    if (s->rm_export_fd >= 0)
        close(s->rm_export_fd);
    if (s->gem) {
        struct drm_gem_close gc = { .handle = s->gem };
        drmIoctl(rm->drm_fd, DRM_IOCTL_GEM_CLOSE, &gc);
    }
    if (s->cpu)
        crm_unmap_memory(rm->c, rm->dev, s->mem, (void *)s->cpu, s->size, 0);
    if (s->mem)
        crm_free(rm->c, rm->dev, s->mem);
    memset(s, 0, sizeof *s);
}

static void fill(const struct layout *l, struct surf *s)
{
    /* Background in the unused tail/padding: a marker. */
    for (uint64_t o = 0; o < s->size; o += 4)
        *(volatile uint32_t *)(s->cpu + o) = 0xdeadbeefu;
    for (uint32_t y = 0; y < H; y++)
        for (uint32_t x = 0; x < W; x++)
            *(volatile uint32_t *)(s->cpu + px_off(l, x, y)) = pat(x, y);
}

/* ---- Vulkan side ---------------------------------------------------------- */

struct vk {
    VkInstance inst;
    VkPhysicalDevice pd;
    VkPhysicalDeviceMemoryProperties mp;
    VkDevice dev;
    uint32_t qf;
    VkQueue q;
    VkCommandPool pool;
    PFN_vkGetMemoryFdPropertiesKHR GetMemoryFdProperties;
    PFN_vkGetImageDrmFormatModifierPropertiesEXT GetImageDrmFormatModifierProperties;
};

static uint32_t find_mem(struct vk *v, uint32_t bits, VkMemoryPropertyFlags want)
{
    for (uint32_t i = 0; i < v->mp.memoryTypeCount; i++)
        if ((bits & (1u << i)) && (v->mp.memoryTypes[i].propertyFlags & want) == want)
            return i;
    return UINT32_MAX;
}

static int vk_init(struct vk *v)
{
    memset(v, 0, sizeof *v);
    VkApplicationInfo ai = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
                             .pApplicationName = "host_import_spike",
                             .apiVersion = VK_API_VERSION_1_3 };
    VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                                 .pApplicationInfo = &ai };
    VkResult r = vkCreateInstance(&ici, NULL, &v->inst);
    report(r == VK_SUCCESS, "vkCreateInstance (%s)", vkr(r));
    if (r)
        return -1;
    uint32_t n = 0;
    vkEnumeratePhysicalDevices(v->inst, &n, NULL);
    VkPhysicalDevice pds[16];
    if (n > 16)
        n = 16;
    vkEnumeratePhysicalDevices(v->inst, &n, pds);
    for (uint32_t i = 0; i < n; i++) {
        VkPhysicalDeviceDriverProperties dp = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRIVER_PROPERTIES };
        VkPhysicalDeviceProperties2 p2 = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2,
                                           .pNext = &dp };
        vkGetPhysicalDeviceProperties2(pds[i], &p2);
        printf("       device %u: %s, driver %s %s (id %d)\n", i, p2.properties.deviceName,
               dp.driverName, dp.driverInfo, dp.driverID);
        if (!v->pd && dp.driverID == VK_DRIVER_ID_NVIDIA_PROPRIETARY)
            v->pd = pds[i];
    }
    report(v->pd != NULL, "NVIDIA proprietary Vulkan device found");
    if (!v->pd)
        return -1;
    vkGetPhysicalDeviceMemoryProperties(v->pd, &v->mp);

    const char *exts[] = {
        VK_KHR_EXTERNAL_MEMORY_FD_EXTENSION_NAME,
        VK_EXT_EXTERNAL_MEMORY_DMA_BUF_EXTENSION_NAME,
        VK_EXT_IMAGE_DRM_FORMAT_MODIFIER_EXTENSION_NAME,
    };
    uint32_t qn = 0;
    vkGetPhysicalDeviceQueueFamilyProperties(v->pd, &qn, NULL);
    VkQueueFamilyProperties qp[16];
    if (qn > 16)
        qn = 16;
    vkGetPhysicalDeviceQueueFamilyProperties(v->pd, &qn, qp);
    v->qf = 0;
    for (uint32_t i = 0; i < qn; i++)
        if (qp[i].queueFlags & VK_QUEUE_GRAPHICS_BIT) {
            v->qf = i;
            break;
        }
    float prio = 1.0f;
    VkDeviceQueueCreateInfo qci = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                                    .queueFamilyIndex = v->qf, .queueCount = 1,
                                    .pQueuePriorities = &prio };
    VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                               .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci,
                               .enabledExtensionCount = 3, .ppEnabledExtensionNames = exts };
    r = vkCreateDevice(v->pd, &dci, NULL, &v->dev);
    report(r == VK_SUCCESS, "vkCreateDevice with external_memory_fd, external_memory_dma_buf, "
           "image_drm_format_modifier (%s)", vkr(r));
    if (r)
        return -1;
    vkGetDeviceQueue(v->dev, v->qf, 0, &v->q);
    VkCommandPoolCreateInfo pci = { .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
                                    .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
                                    .queueFamilyIndex = v->qf };
    vkCreateCommandPool(v->dev, &pci, NULL, &v->pool);
    v->GetMemoryFdProperties = (PFN_vkGetMemoryFdPropertiesKHR)
        vkGetDeviceProcAddr(v->dev, "vkGetMemoryFdPropertiesKHR");
    v->GetImageDrmFormatModifierProperties = (PFN_vkGetImageDrmFormatModifierPropertiesEXT)
        vkGetDeviceProcAddr(v->dev, "vkGetImageDrmFormatModifierPropertiesEXT");
    return 0;
}

static void list_modifiers(struct vk *v, VkFormat fmt, const char *fname)
{
    VkDrmFormatModifierPropertiesListEXT ml = {
        .sType = VK_STRUCTURE_TYPE_DRM_FORMAT_MODIFIER_PROPERTIES_LIST_EXT };
    VkFormatProperties2 fp = { .sType = VK_STRUCTURE_TYPE_FORMAT_PROPERTIES_2, .pNext = &ml };
    vkGetPhysicalDeviceFormatProperties2(v->pd, fmt, &fp);
    VkDrmFormatModifierPropertiesEXT mods[64];
    if (ml.drmFormatModifierCount > 64)
        ml.drmFormatModifierCount = 64;
    ml.pDrmFormatModifierProperties = mods;
    vkGetPhysicalDeviceFormatProperties2(v->pd, fmt, &fp);
    printf("       %s: %u DRM format modifiers advertised\n", fname, ml.drmFormatModifierCount);
    for (uint32_t i = 0; i < ml.drmFormatModifierCount; i++) {
        const uint64_t m = mods[i].drmFormatModifier;
        printf("         0x%016" PRIx64 " planes %u features 0x%08x", m,
               mods[i].drmFormatModifierPlaneCount, mods[i].drmFormatModifierTilingFeatures);
        if (m == DRM_FORMAT_MOD_LINEAR)
            printf("  LINEAR");
        else if ((m >> 56) == DRM_FORMAT_MOD_VENDOR_NVIDIA && (m & 0x10))
            printf("  BL2D h=%u kind=0x%02x gen=%u sector=%u compr=%u", (unsigned)(m & 0xf),
                   (unsigned)((m >> 12) & 0xff), (unsigned)((m >> 20) & 3),
                   (unsigned)(((m >> 22) & 1) | (((m >> 26) & 3) << 1)),
                   (unsigned)((m >> 23) & 7));
        printf("\n");
    }
}

/* Image format support for (tiling, modifier, handle type). */
static VkResult query_ifp(struct vk *v, VkImageTiling tiling, uint64_t mod,
                          VkExternalMemoryHandleTypeFlagBits ht, VkExternalMemoryProperties *emp)
{
    VkPhysicalDeviceImageDrmFormatModifierInfoEXT mi = {
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_DRM_FORMAT_MODIFIER_INFO_EXT,
        .drmFormatModifier = mod, .sharingMode = VK_SHARING_MODE_EXCLUSIVE };
    VkPhysicalDeviceExternalImageFormatInfo ei = {
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_IMAGE_FORMAT_INFO,
        .pNext = tiling == VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT ? &mi : NULL,
        .handleType = ht };
    VkPhysicalDeviceImageFormatInfo2 fi = {
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2, .pNext = &ei,
        .format = VK_FORMAT_B8G8R8A8_UNORM, .type = VK_IMAGE_TYPE_2D, .tiling = tiling,
        .usage = VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT |
                 VK_IMAGE_USAGE_SAMPLED_BIT };
    VkExternalImageFormatProperties eip = {
        .sType = VK_STRUCTURE_TYPE_EXTERNAL_IMAGE_FORMAT_PROPERTIES };
    VkImageFormatProperties2 ip = { .sType = VK_STRUCTURE_TYPE_IMAGE_FORMAT_PROPERTIES_2,
                                    .pNext = &eip };
    VkResult r = vkGetPhysicalDeviceImageFormatProperties2(v->pd, &fi, &ip);
    if (emp)
        *emp = eip.externalMemoryProperties;
    return r;
}

static VkCommandBuffer begin(struct vk *v)
{
    VkCommandBufferAllocateInfo ai = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
                                       .commandPool = v->pool,
                                       .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
                                       .commandBufferCount = 1 };
    VkCommandBuffer cb;
    vkAllocateCommandBuffers(v->dev, &ai, &cb);
    VkCommandBufferBeginInfo bi = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
                                    .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT };
    vkBeginCommandBuffer(cb, &bi);
    return cb;
}

static VkResult submit(struct vk *v, VkCommandBuffer cb)
{
    vkEndCommandBuffer(cb);
    VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1,
                        .pCommandBuffers = &cb };
    VkResult r = vkQueueSubmit(v->q, 1, &si, VK_NULL_HANDLE);
    if (r == VK_SUCCESS)
        r = vkQueueWaitIdle(v->q);
    vkFreeCommandBuffers(v->dev, v->pool, 1, &cb);
    return r;
}

/* Acquire from / release to the external owner (the RM-side view). */
static void barrier(VkCommandBuffer cb, VkImage img, uint32_t qf, bool acquire)
{
    VkImageMemoryBarrier b = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
        .srcAccessMask = acquire ? 0 : VK_ACCESS_TRANSFER_WRITE_BIT,
        .dstAccessMask = acquire ? VK_ACCESS_TRANSFER_READ_BIT | VK_ACCESS_TRANSFER_WRITE_BIT : 0,
        .oldLayout = VK_IMAGE_LAYOUT_GENERAL,
        .newLayout = VK_IMAGE_LAYOUT_GENERAL,
        .srcQueueFamilyIndex = acquire ? VK_QUEUE_FAMILY_EXTERNAL : qf,
        .dstQueueFamilyIndex = acquire ? qf : VK_QUEUE_FAMILY_EXTERNAL,
        .image = img,
        .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 },
    };
    vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT,
                         0, 0, NULL, 0, NULL, 1, &b);
}

struct hbuf {
    VkBuffer buf;
    VkDeviceMemory mem;
    void *map;
};

static int hbuf_create(struct vk *v, VkDeviceSize size, struct hbuf *b)
{
    VkBufferCreateInfo bci = { .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size = size,
                               .usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT |
                                        VK_BUFFER_USAGE_TRANSFER_DST_BIT };
    if (vkCreateBuffer(v->dev, &bci, NULL, &b->buf))
        return -1;
    VkMemoryRequirements mr;
    vkGetBufferMemoryRequirements(v->dev, b->buf, &mr);
    VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                                 .allocationSize = mr.size,
                                 .memoryTypeIndex = find_mem(v, mr.memoryTypeBits,
                                     VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                                     VK_MEMORY_PROPERTY_HOST_COHERENT_BIT) };
    if (vkAllocateMemory(v->dev, &mai, NULL, &b->mem))
        return -1;
    vkBindBufferMemory(v->dev, b->buf, b->mem, 0);
    vkMapMemory(v->dev, b->mem, 0, VK_WHOLE_SIZE, 0, &b->map);
    return 0;
}

static void hbuf_destroy(struct vk *v, struct hbuf *b)
{
    if (b->buf)
        vkDestroyBuffer(v->dev, b->buf, NULL);
    if (b->mem)
        vkFreeMemory(v->dev, b->mem, NULL);
    memset(b, 0, sizeof *b);
}

/* Create a modifier image with an explicit single-plane layout. */
static VkResult make_image(struct vk *v, uint32_t w, uint32_t h, uint64_t mod, uint64_t offset,
                           uint64_t row_pitch, uint64_t size, VkImage *img)
{
    VkSubresourceLayout pl = { .offset = offset, .size = size, .rowPitch = row_pitch };
    VkImageDrmFormatModifierExplicitCreateInfoEXT xi = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT,
        .drmFormatModifier = mod, .drmFormatModifierPlaneCount = 1, .pPlaneLayouts = &pl };
    VkExternalMemoryImageCreateInfo emi = {
        .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO, .pNext = &xi,
        .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT };
    VkImageCreateInfo ici = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO, .pNext = &emi,
        .imageType = VK_IMAGE_TYPE_2D, .format = VK_FORMAT_B8G8R8A8_UNORM,
        .extent = { w, h, 1 }, .mipLevels = 1, .arrayLayers = 1,
        .samples = VK_SAMPLE_COUNT_1_BIT, .tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT,
        .usage = VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT |
                 VK_IMAGE_USAGE_SAMPLED_BIT,
        .sharingMode = VK_SHARING_MODE_EXCLUSIVE, .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED };
    return vkCreateImage(v->dev, &ici, NULL, img);
}

/* Import fd (dup'd; the import takes ownership on success) for img. */
static VkResult import_bind(struct vk *v, VkImage img, int fd, VkExternalMemoryHandleTypeFlagBits ht,
                            uint64_t fd_size, VkDeviceMemory *mem, bool verbose)
{
    VkMemoryDedicatedRequirements dr = { .sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_REQUIREMENTS };
    VkMemoryRequirements2 mr2 = { .sType = VK_STRUCTURE_TYPE_MEMORY_REQUIREMENTS_2, .pNext = &dr };
    VkImageMemoryRequirementsInfo2 ri = { .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_REQUIREMENTS_INFO_2,
                                          .image = img };
    vkGetImageMemoryRequirements2(v->dev, &ri, &mr2);
    uint32_t bits = mr2.memoryRequirements.memoryTypeBits;
    VkMemoryFdPropertiesKHR fp = { .sType = VK_STRUCTURE_TYPE_MEMORY_FD_PROPERTIES_KHR };
    VkResult r = VK_SUCCESS;
    if (ht == VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT) {
        r = v->GetMemoryFdProperties(v->dev, ht, fd, &fp);
        if (verbose)
            printf("       vkGetMemoryFdPropertiesKHR(DMA_BUF): %s, memoryTypeBits 0x%x\n", vkr(r),
                   fp.memoryTypeBits);
        if (r == VK_SUCCESS)
            bits &= fp.memoryTypeBits;
    }
    if (verbose)
        printf("       image memory requirements: size 0x%" PRIx64 " align 0x%" PRIx64
               " types 0x%x, dedicated prefers %u requires %u; fd size 0x%" PRIx64 "\n",
               (uint64_t)mr2.memoryRequirements.size, (uint64_t)mr2.memoryRequirements.alignment,
               mr2.memoryRequirements.memoryTypeBits, dr.prefersDedicatedAllocation,
               dr.requiresDedicatedAllocation, fd_size);
    uint32_t mt = find_mem(v, bits, 0);
    if (mt == UINT32_MAX) {
        if (verbose)
            printf("       no memory type in 0x%x\n", bits);
        return VK_ERROR_INVALID_EXTERNAL_HANDLE;
    }
    int dfd = dup(fd);
    VkMemoryDedicatedAllocateInfo dai = { .sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
                                          .image = img };
    VkImportMemoryFdInfoKHR imp = { .sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR,
                                    .pNext = &dai, .handleType = ht, .fd = dfd };
    VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .pNext = &imp,
                                 .allocationSize = mr2.memoryRequirements.size,
                                 .memoryTypeIndex = mt };
    r = vkAllocateMemory(v->dev, &mai, NULL, mem);
    if (r != VK_SUCCESS) {
        close(dfd);
        return r;
    }
    r = vkBindImageMemory(v->dev, img, *mem, 0);
    return r;
}

/* ---- the per-layout test -------------------------------------------------- */

static void test_layout(struct rm *rm, struct vk *v, const struct layout *l)
{
    printf("\n== %s: modifier 0x%016" PRIx64 ", pitch %u, size 0x%" PRIx64 " ==\n", l->name,
           l->modifier, l->pitch, l->size);
    VkExternalMemoryProperties emp;
    VkResult r = query_ifp(v, VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT, l->modifier,
                           VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT, &emp);
    report(r == VK_SUCCESS, "  image format properties (modifier, DMA_BUF): %s, features 0x%x "
           "compatible 0x%x", vkr(r), emp.externalMemoryFeatures, emp.compatibleHandleTypes);

    struct surf s;
    if (rm_alloc(rm, l->size, &s))
        goto out;
    fill(l, &s);
    if (rm_export(rm, l, &s))
        goto out;

    /* Try plane-layout variants until one creates and imports. */
    const uint64_t pitches[] = { l->pitch, 0 };
    VkImage img = VK_NULL_HANDLE;
    VkDeviceMemory mem = VK_NULL_HANDLE;
    off_t dsz = lseek(s.dmabuf_fd, 0, SEEK_END);
    for (unsigned i = 0; i < 2 && !mem; i++) {
        r = make_image(v, W, H, l->modifier, 0, pitches[i], l->size, &img);
        report(r == VK_SUCCESS, "  vkCreateImage explicit modifier, offset 0, rowPitch %" PRIu64
               ", size 0x%" PRIx64 ": %s", pitches[i], l->size, vkr(r));
        if (r)
            continue;
        r = import_bind(v, img, s.dmabuf_fd, VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
                        (uint64_t)dsz, &mem, true);
        report(r == VK_SUCCESS, "  import dma-buf + bind: %s", vkr(r));
        if (r) {
            if (mem)
                vkFreeMemory(v->dev, mem, NULL);
            mem = VK_NULL_HANDLE;
            vkDestroyImage(v->dev, img, NULL);
            img = VK_NULL_HANDLE;
        }
    }
    if (!mem)
        goto out;

    VkImageDrmFormatModifierPropertiesEXT ip = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_PROPERTIES_EXT };
    v->GetImageDrmFormatModifierProperties(v->dev, img, &ip);
    VkImageSubresource sr = { VK_IMAGE_ASPECT_MEMORY_PLANE_0_BIT_EXT, 0, 0 };
    VkSubresourceLayout sl;
    vkGetImageSubresourceLayout(v->dev, img, &sr, &sl);
    printf("       driver's view: modifier 0x%016" PRIx64 ", plane 0 offset %" PRIu64 " size 0x%"
           PRIx64 " rowPitch %" PRIu64 "\n", ip.drmFormatModifier, (uint64_t)sl.offset,
           (uint64_t)sl.size, (uint64_t)sl.rowPitch);

    struct hbuf hb = { 0 };
    if (hbuf_create(v, (VkDeviceSize)W * H * BPP, &hb)) {
        report(false, "  readback buffer");
        goto out_img;
    }

    /* Read check: whole image -> buffer, compare. */
    {
        VkCommandBuffer cb = begin(v);
        barrier(cb, img, v->qf, true);
        VkBufferImageCopy c = { .imageSubresource = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1 },
                                .imageExtent = { W, H, 1 } };
        vkCmdCopyImageToBuffer(cb, img, VK_IMAGE_LAYOUT_GENERAL, hb.buf, 1, &c);
        barrier(cb, img, v->qf, false);
        r = submit(v, cb);
        const uint32_t *px = hb.map;
        uint64_t bad = 0;
        uint32_t fx = 0, fy = 0, fv = 0;
        for (uint32_t y = 0; y < H; y++)
            for (uint32_t x = 0; x < W; x++)
                if (px[y * W + x] != pat(x, y)) {
                    if (!bad++) {
                        fx = x;
                        fy = y;
                        fv = px[y * W + x];
                    }
                }
        report(r == VK_SUCCESS && bad == 0,
               "  READ: NVIDIA Vulkan copy of the RM-written image: %s, %" PRIu64
               " of %u pixels wrong", vkr(r), bad, W * H);
        if (bad)
            printf("       first wrong pixel (%u,%u): got 0x%08x (= pixel (%u,%u) of the pattern)\n",
                   fx, fy, fv, fv & 0xffff, fv >> 16);
    }

    /* Write check: a 96x40 block at (200, 236): crosses GOB columns, GOB
     * rows and (for h<=5) a block row boundary at y=256. */
    {
        const uint32_t rx = 200, ry = 236, rw = 96, rh = 40;
        uint32_t *src = hb.map;
        for (uint32_t y = 0; y < rh; y++)
            for (uint32_t x = 0; x < rw; x++)
                src[y * rw + x] = pat2(rx + x, ry + y);
        VkCommandBuffer cb = begin(v);
        barrier(cb, img, v->qf, true);
        VkBufferImageCopy c = { .bufferRowLength = rw,
                                .imageSubresource = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1 },
                                .imageOffset = { (int32_t)rx, (int32_t)ry, 0 },
                                .imageExtent = { rw, rh, 1 } };
        vkCmdCopyBufferToImage(cb, hb.buf, img, VK_IMAGE_LAYOUT_GENERAL, 1, &c);
        barrier(cb, img, v->qf, false);
        r = submit(v, cb);
        uint64_t bad_in = 0, bad_out = 0;
        for (uint32_t y = ry - 16; y < ry + rh + 16; y++)
            for (uint32_t x = rx - 64; x < rx + rw + 64; x++) {
                const uint32_t got = *(volatile uint32_t *)(s.cpu + px_off(l, x, y));
                const bool in = x >= rx && x < rx + rw && y >= ry && y < ry + rh;
                if (got != (in ? pat2(x, y) : pat(x, y))) {
                    if (in)
                        bad_in++;
                    else
                        bad_out++;
                    if (bad_in + bad_out <= 3)
                        printf("       RM view (%u,%u): 0x%08x, want 0x%08x\n", x, y, got,
                               in ? pat2(x, y) : pat(x, y));
                }
            }
        /* A sparse sample of the rest of the image, the tail marker too. */
        for (uint32_t y = 0; y < H; y += 7)
            for (uint32_t x = 0; x < W; x += 13) {
                const bool near = x + 64 >= rx && x < rx + rw + 64 && y + 16 >= ry &&
                                  y < ry + rh + 16;
                if (!near && *(volatile uint32_t *)(s.cpu + px_off(l, x, y)) != pat(x, y))
                    bad_out++;
            }
        report(r == VK_SUCCESS && bad_in == 0 && bad_out == 0,
               "  WRITE: NVIDIA Vulkan copy into (%u,%u) %ux%u, checked through the RM CPU "
               "mapping: %s, %" PRIu64 " wrong inside, %" PRIu64 " wrong outside",
               rx, ry, rw, rh, vkr(r), bad_in, bad_out);
    }

    hbuf_destroy(v, &hb);
out_img:
    vkDestroyImage(v->dev, img, NULL);
    vkFreeMemory(v->dev, mem, NULL);
out:
    rm_free(rm, &s);
}

/* Plane-layout constraints, without any memory: which rowPitch / offset the
 * driver accepts at vkCreateImage for a 1366x768 LINEAR image (5464 B rows). */
static void probe_linear_layouts(struct vk *v)
{
    printf("\n== LINEAR plane-layout probes (1366x768, no memory) ==\n");
    const uint64_t pitches[] = { 5464, 5472, 5504, 5632, 6144 };
    const uint64_t offsets[] = { 0, 256, 4096, 65536 };
    for (unsigned i = 0; i < sizeof pitches / sizeof pitches[0]; i++) {
        VkImage img;
        VkResult r = make_image(v, 1366, 768, DRM_FORMAT_MOD_LINEAR, 0, pitches[i],
                                pitches[i] * 768, &img);
        printf("       rowPitch %5" PRIu64 " (%% 256 = %3" PRIu64 "): %s\n", pitches[i],
               pitches[i] % 256, vkr(r));
        if (r == VK_SUCCESS)
            vkDestroyImage(v->dev, img, NULL);
    }
    for (unsigned i = 0; i < sizeof offsets / sizeof offsets[0]; i++) {
        VkImage img;
        VkResult r = make_image(v, 1366, 768, DRM_FORMAT_MOD_LINEAR, offsets[i], 5632, 5632 * 768,
                                &img);
        printf("       offset %6" PRIu64 " (rowPitch 5632): %s\n", offsets[i], vkr(r));
        if (r == VK_SUCCESS)
            vkDestroyImage(v->dev, img, NULL);
    }
}

/* The "opaque" routes: OPTIMAL tiling with OPAQUE_FD / DMA_BUF handle types,
 * fed the nvidia-drm dma-buf or the raw RM export fd. */
static void test_opaque(struct rm *rm, struct vk *v)
{
    printf("\n== opaque routes (OPTIMAL tiling) ==\n");
    VkExternalMemoryProperties emp;
    VkResult r = query_ifp(v, VK_IMAGE_TILING_OPTIMAL, 0,
                           VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT, &emp);
    printf("       image format properties OPTIMAL + DMA_BUF: %s, features 0x%x\n", vkr(r),
           emp.externalMemoryFeatures);
    r = query_ifp(v, VK_IMAGE_TILING_OPTIMAL, 0, VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT, &emp);
    printf("       image format properties OPTIMAL + OPAQUE_FD: %s, features 0x%x compatible 0x%x\n",
           vkr(r), emp.externalMemoryFeatures, emp.compatibleHandleTypes);

    struct layout l;
    layout_init(&l, "opaque", DRM_FORMAT_MOD_NVIDIA_BLOCK_LINEAR_2D(0, 1, 2, 0x06, 4));
    struct surf s;
    if (rm_alloc(rm, l.size + (1u << 20), &s) || rm_export(rm, &l, &s)) {
        rm_free(rm, &s);
        return;
    }
    off_t dsz = lseek(s.dmabuf_fd, 0, SEEK_END);

    struct {
        const char *what;
        VkExternalMemoryHandleTypeFlagBits ht;
        int fd;
    } cases[] = {
        { "dma-buf as OPAQUE_FD", VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT, s.dmabuf_fd },
        { "dma-buf as DMA_BUF", VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT, s.dmabuf_fd },
        { "RM export fd (/dev/nvidiactl) as OPAQUE_FD", VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT,
          s.rm_export_fd },
    };
    for (unsigned i = 0; i < sizeof cases / sizeof cases[0]; i++) {
        if (cases[i].fd < 0)
            continue;
        VkExternalMemoryImageCreateInfo emi = {
            .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
            .handleTypes = cases[i].ht };
        VkImageCreateInfo ici = {
            .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO, .pNext = &emi,
            .imageType = VK_IMAGE_TYPE_2D, .format = VK_FORMAT_B8G8R8A8_UNORM,
            .extent = { W, H, 1 }, .mipLevels = 1, .arrayLayers = 1,
            .samples = VK_SAMPLE_COUNT_1_BIT, .tiling = VK_IMAGE_TILING_OPTIMAL,
            .usage = VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_SAMPLED_BIT,
            .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED };
        VkImage img;
        r = vkCreateImage(v->dev, &ici, NULL, &img);
        if (r) {
            printf("       %s: vkCreateImage %s\n", cases[i].what, vkr(r));
            continue;
        }
        VkDeviceMemory mem = VK_NULL_HANDLE;
        printf("       %s:\n", cases[i].what);
        r = import_bind(v, img, cases[i].fd, cases[i].ht, (uint64_t)dsz, &mem, true);
        printf("       -> %s\n", vkr(r));
        vkDestroyImage(v->dev, img, NULL);
        if (mem)
            vkFreeMemory(v->dev, mem, NULL);
    }
    rm_free(rm, &s);
}

int main(int argc, char **argv)
{
    const char *node = argc > 1 ? argv[1] : "/dev/dri/renderD128";
    struct rm rm = { .drm_fd = -1 };
    struct vk v;

    int r = crm_open(&rm.c, NULL);
    report(r == 0, "crm_open (%s), RM %s", crm_status_name(r), r ? "-" : crm_rm_version(rm.c));
    if (r)
        return 1;
    NV0080_ALLOC_PARAMETERS dp;
    memset(&dp, 0, sizeof dp);
    r = crm_alloc(rm.c, crm_root(rm.c), &rm.dev, NV01_DEVICE_0, &dp, sizeof dp);
    report(r == 0, "alloc NV01_DEVICE_0 (%s)", crm_status_name(r));
    NV2080_ALLOC_PARAMETERS sp = { .subDeviceId = 0 };
    if (!r)
        r = crm_alloc(rm.c, rm.dev, &rm.sub, NV20_SUBDEVICE_0, &sp, sizeof sp);
    if (r)
        goto out;

    rm.drm_fd = open(node, O_RDWR | O_CLOEXEC);
    report(rm.drm_fd >= 0, "open %s", node);
    if (rm.drm_fd < 0)
        goto out;
    drmVersionPtr ver = drmGetVersion(rm.drm_fd);
    struct drm_nvidia_get_dev_info_params di;
    memset(&di, 0, sizeof di);
    int dr = drmIoctl(rm.drm_fd, DRM_IOCTL_NVIDIA_GET_DEV_INFO, &di);
    printf("       DRM driver %s; DRM_NVIDIA_GET_DEV_INFO %s: gpu_id 0x%x generic_page_kind 0x%x "
           "page_kind_generation %u sector_layout %u\n", ver ? ver->name : "?",
           dr ? "failed" : "ok", di.gpu_id, di.generic_page_kind, di.page_kind_generation,
           di.sector_layout);
    drmFreeVersion(ver);

    if (vk_init(&v))
        goto out;
    printf("\n");
    list_modifiers(&v, VK_FORMAT_B8G8R8A8_UNORM, "VK_FORMAT_B8G8R8A8_UNORM");
    list_modifiers(&v, VK_FORMAT_R8G8B8A8_UNORM, "VK_FORMAT_R8G8B8A8_UNORM");

    /* What NIL (NVK) advertises for B8G8R8A8 on GB202: kind 0x06, gob kind
     * generation 2 (Turing), sector layout 1 (desktop), uncompressed,
     * h = 5..0, plus LINEAR; for a 1920x1080 swapchain image it picks h=5. */
    struct layout ls[4];
    layout_init(&ls[0], "LINEAR", DRM_FORMAT_MOD_LINEAR);
    layout_init(&ls[1], "BL h=5 (NVK swapchain default)",
                DRM_FORMAT_MOD_NVIDIA_BLOCK_LINEAR_2D(0, 1, 2, 0x06, 5));
    layout_init(&ls[2], "BL h=4", DRM_FORMAT_MOD_NVIDIA_BLOCK_LINEAR_2D(0, 1, 2, 0x06, 4));
    layout_init(&ls[3], "BL h=0", DRM_FORMAT_MOD_NVIDIA_BLOCK_LINEAR_2D(0, 1, 2, 0x06, 0));
    for (unsigned i = 0; i < 4; i++)
        test_layout(&rm, &v, &ls[i]);

    probe_linear_layouts(&v);
    test_opaque(&rm, &v);

    vkDestroyCommandPool(v.dev, v.pool, NULL);
    vkDestroyDevice(v.dev, NULL);
    vkDestroyInstance(v.inst, NULL);
out:
    if (rm.drm_fd >= 0)
        close(rm.drm_fd);
    if (rm.sub)
        crm_free(rm.c, rm.dev, rm.sub);
    if (rm.dev)
        crm_free(rm.c, crm_root(rm.c), rm.dev);
    crm_close(rm.c);
    printf("\n%s (%d failure%s)\n", failures ? "HOST IMPORT SPIKE: FAILURES" : "HOST IMPORT SPIKE: ALL OK",
           failures, failures == 1 ? "" : "s");
    return failures ? 1 : 0;
}
