/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * nb_vkpresent.c -- see nb_vkpresent.h.
 *
 * One device, one queue, one command buffer, one swapchain.  Per guest frame:
 * acquire (zero timeout), one command buffer that clears the bars and blits
 * the guest's image into the swapchain image, submit (waiting on the guest
 * buffer's implicit fence when the kernel exports it), present, wait for the
 * submit's fence.  Guest buffers are imported once per slot
 * (VK_EXT_image_drm_format_modifier + VK_EXT_external_memory_dma_buf) and
 * kept until the slot is dropped.
 */
#include <errno.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <time.h>
#include <unistd.h>

#include "nb_vkpresent.h"

static bool probe_done, probe_ok;
static const char *probe_why;

void nb_vkp_env(void)
{
    setenv(NB_VKP_ENV, "1", 1);
}

#ifndef NB_HAVE_VULKAN

bool nb_vkp_available(const char **why)
{
    if (why) {
        *why = "the viewer was built without the Vulkan headers";
    }
    (void)probe_done; (void)probe_ok; (void)probe_why;
    return false;
}

struct nb_vkp *nb_vkp_open_wayland(void *d, void *s, char *why, size_t n)
{
    (void)d; (void)s;
    snprintf(why, n, "built without Vulkan");
    return NULL;
}

struct nb_vkp *nb_vkp_open_xcb(void *c, uint32_t w, char *why, size_t n)
{
    (void)c; (void)w;
    snprintf(why, n, "built without Vulkan");
    return NULL;
}

void nb_vkp_close(struct nb_vkp *v) { (void)v; }
const char *nb_vkp_device(const struct nb_vkp *v) { (void)v; return ""; }
int nb_vkp_present(struct nb_vkp *v, const struct nb_vkp_frame *f,
                   unsigned t, unsigned *g)
{
    (void)v; (void)f; (void)t; (void)g;
    return -ENOTSUP;
}
void nb_vkp_forget(struct nb_vkp *v, int key) { (void)v; (void)key; }

#else /* NB_HAVE_VULKAN */

#include <dlfcn.h>
#if __has_include(<linux/dma-buf.h>)
#include <linux/dma-buf.h>
#endif

#define VK_NO_PROTOTYPES
#ifdef NB_HAVE_WAYLAND
#define VK_USE_PLATFORM_WAYLAND_KHR
#endif
#ifdef NB_HAVE_X11
#define VK_USE_PLATFORM_XCB_KHR
#endif
#include <vulkan/vulkan.h>

bool nb_vkp_available(const char **why)
{
    if (!probe_done) {
        probe_done = true;
        if (!nb_vkp_gpu_present(NULL)) {
            probe_why = "no NVIDIA RTX 40 or 50 series GPU";
        } else if (!nb_vkp_layer_present(NULL)) {
            probe_why = "the NVIDIA driver's present layer (" NB_VKP_LAYER
                        ") is not installed";
        } else if (!dlopen("libvulkan.so.1", RTLD_NOW | RTLD_LOCAL)) {
            probe_why = "no libvulkan.so.1";
        } else {
            probe_ok = true;
        }
    }
    if (why) {
        *why = probe_ok ? NULL : probe_why;
    }
    return probe_ok;
}

#define NB_VKP_IMAGES 8
#define NB_VKP_SRC    8

struct vk_fns {
#define F(n) PFN_##n n
    F(vkGetInstanceProcAddr);
    F(vkCreateInstance);
    F(vkDestroyInstance);
    F(vkEnumeratePhysicalDevices);
    F(vkGetPhysicalDeviceProperties);
    F(vkGetPhysicalDeviceQueueFamilyProperties);
    F(vkGetPhysicalDeviceMemoryProperties);
    F(vkEnumerateDeviceExtensionProperties);
    F(vkCreateDevice);
    F(vkGetDeviceProcAddr);
    F(vkDestroySurfaceKHR);
    F(vkGetPhysicalDeviceSurfaceSupportKHR);
    F(vkGetPhysicalDeviceSurfaceCapabilitiesKHR);
    F(vkGetPhysicalDeviceSurfaceFormatsKHR);
    F(vkGetPhysicalDeviceSurfacePresentModesKHR);
#ifdef NB_HAVE_WAYLAND
    F(vkCreateWaylandSurfaceKHR);
#endif
#ifdef NB_HAVE_X11
    F(vkCreateXcbSurfaceKHR);
#endif
    F(vkDestroyDevice);
    F(vkGetDeviceQueue);
    F(vkCreateSwapchainKHR);
    F(vkDestroySwapchainKHR);
    F(vkGetSwapchainImagesKHR);
    F(vkAcquireNextImageKHR);
    F(vkQueuePresentKHR);
    F(vkCreateCommandPool);
    F(vkDestroyCommandPool);
    F(vkAllocateCommandBuffers);
    F(vkBeginCommandBuffer);
    F(vkEndCommandBuffer);
    F(vkResetCommandBuffer);
    F(vkCmdPipelineBarrier);
    F(vkCmdBlitImage);
    F(vkCmdClearColorImage);
    F(vkQueueSubmit);
    F(vkCreateFence);
    F(vkDestroyFence);
    F(vkWaitForFences);
    F(vkResetFences);
    F(vkCreateSemaphore);
    F(vkDestroySemaphore);
    F(vkCreateImage);
    F(vkDestroyImage);
    F(vkGetImageMemoryRequirements);
    F(vkAllocateMemory);
    F(vkFreeMemory);
    F(vkBindImageMemory);
    F(vkGetMemoryFdPropertiesKHR);
    F(vkImportSemaphoreFdKHR);
    F(vkDeviceWaitIdle);
#undef F
};

struct nb_vkp {
    struct vk_fns f;
    void *lib;
    VkInstance inst;
    VkPhysicalDevice pd;
    VkDevice dev;
    VkQueue q;
    uint32_t qf;
    VkSurfaceKHR surf;
    VkSwapchainKHR sc;
    VkFormat sc_fmt;
    VkExtent2D sc_ext;
    uint32_t req_w, req_h;      /* the size asked for (X11 may differ)   */
    int sc_mode;
    uint32_t n_img;
    VkImage img[NB_VKP_IMAGES];
    VkSemaphore done[NB_VKP_IMAGES];
    /* Per swapchain image: presented before with black bars around this
     * destination, so it is reused as it is (PRESENT_SRC, no clear). */
    struct { bool ok; int x, y, w, h; } bars[NB_VKP_IMAGES];
    bool low_prio;              /* the queue runs at LOW global priority */
    VkSemaphore acq;
    VkSemaphore guest_ready;    /* the guest buffer's implicit fence     */
    VkFence fence;
    VkCommandPool pool;
    VkCommandBuffer cb;
    bool foreign;               /* VK_EXT_queue_family_foreign           */
    bool sync_fd;               /* VK_KHR_external_semaphore_fd          */
    VkPhysicalDeviceMemoryProperties mem;
    char name[VK_MAX_PHYSICAL_DEVICE_NAME_SIZE];
    struct {
        bool valid;
        int key;
        uint32_t w, h, fourcc;
        uint64_t mod;
        VkImage img;
        VkDeviceMemory mem;
    } s[NB_VKP_SRC];
};

static void say(char *why, size_t n, const char *msg, VkResult r)
{
    if (why && n) {
        if (r != VK_SUCCESS) {
            snprintf(why, n, "%s (VkResult %d)", msg, (int)r);
        } else {
            snprintf(why, n, "%s", msg);
        }
    }
}

static VkFormat fourcc_fmt(uint32_t f)
{
#define FC(a, b, c, d) ((uint32_t)(a) | ((uint32_t)(b) << 8) | \
                        ((uint32_t)(c) << 16) | ((uint32_t)(d) << 24))
    switch (f) {
    case FC('X', 'R', '2', '4'):
    case FC('A', 'R', '2', '4'): return VK_FORMAT_B8G8R8A8_UNORM;
    case FC('X', 'B', '2', '4'):
    case FC('A', 'B', '2', '4'): return VK_FORMAT_R8G8B8A8_UNORM;
    case FC('X', 'R', '3', '0'):
    case FC('A', 'R', '3', '0'): return VK_FORMAT_A2R10G10B10_UNORM_PACK32;
    case FC('X', 'B', '3', '0'):
    case FC('A', 'B', '3', '0'): return VK_FORMAT_A2B10G10R10_UNORM_PACK32;
    default:                     return VK_FORMAT_UNDEFINED;
    }
#undef FC
}

static bool has_dev_ext(struct nb_vkp *v, VkExtensionProperties *e, uint32_t n,
                        const char *name)
{
    (void)v;
    for (uint32_t i = 0; i < n; i++) {
        if (!strcmp(e[i].extensionName, name)) {
            return true;
        }
    }
    return false;
}

static bool load_global(struct nb_vkp *v, char *why, size_t n)
{
    v->lib = dlopen("libvulkan.so.1", RTLD_NOW | RTLD_LOCAL);
    if (!v->lib) {
        say(why, n, "cannot load libvulkan.so.1", VK_SUCCESS);
        return false;
    }
    *(void **)&v->f.vkGetInstanceProcAddr = dlsym(v->lib,
                                                  "vkGetInstanceProcAddr");
    if (!v->f.vkGetInstanceProcAddr) {
        say(why, n, "libvulkan lacks vkGetInstanceProcAddr", VK_SUCCESS);
        return false;
    }
    v->f.vkCreateInstance = (PFN_vkCreateInstance)
        v->f.vkGetInstanceProcAddr(NULL, "vkCreateInstance");
    return v->f.vkCreateInstance != NULL;
}

static bool load_instance(struct nb_vkp *v)
{
    struct vk_fns *f = &v->f;
#define I(n) do { f->n = (PFN_##n)f->vkGetInstanceProcAddr(v->inst, #n); \
        if (!f->n) { return false; } } while (0)
    I(vkDestroyInstance);
    I(vkEnumeratePhysicalDevices);
    I(vkGetPhysicalDeviceProperties);
    I(vkGetPhysicalDeviceQueueFamilyProperties);
    I(vkGetPhysicalDeviceMemoryProperties);
    I(vkEnumerateDeviceExtensionProperties);
    I(vkCreateDevice);
    I(vkGetDeviceProcAddr);
    I(vkDestroySurfaceKHR);
    I(vkGetPhysicalDeviceSurfaceSupportKHR);
    I(vkGetPhysicalDeviceSurfaceCapabilitiesKHR);
    I(vkGetPhysicalDeviceSurfaceFormatsKHR);
    I(vkGetPhysicalDeviceSurfacePresentModesKHR);
#undef I
    return true;
}

static bool load_device(struct nb_vkp *v)
{
    struct vk_fns *f = &v->f;
#define D(n) do { f->n = (PFN_##n)f->vkGetDeviceProcAddr(v->dev, #n); \
        if (!f->n) { return false; } } while (0)
    D(vkDestroyDevice);
    D(vkGetDeviceQueue);
    D(vkCreateSwapchainKHR);
    D(vkDestroySwapchainKHR);
    D(vkGetSwapchainImagesKHR);
    D(vkAcquireNextImageKHR);
    D(vkQueuePresentKHR);
    D(vkCreateCommandPool);
    D(vkDestroyCommandPool);
    D(vkAllocateCommandBuffers);
    D(vkBeginCommandBuffer);
    D(vkEndCommandBuffer);
    D(vkResetCommandBuffer);
    D(vkCmdPipelineBarrier);
    D(vkCmdBlitImage);
    D(vkCmdClearColorImage);
    D(vkQueueSubmit);
    D(vkCreateFence);
    D(vkDestroyFence);
    D(vkWaitForFences);
    D(vkResetFences);
    D(vkCreateSemaphore);
    D(vkDestroySemaphore);
    D(vkCreateImage);
    D(vkDestroyImage);
    D(vkGetImageMemoryRequirements);
    D(vkAllocateMemory);
    D(vkFreeMemory);
    D(vkBindImageMemory);
    D(vkGetMemoryFdPropertiesKHR);
    D(vkDeviceWaitIdle);
#undef D
    f->vkImportSemaphoreFdKHR = (PFN_vkImportSemaphoreFdKHR)
        f->vkGetDeviceProcAddr(v->dev, "vkImportSemaphoreFdKHR");
    return true;
}

/* The instance, with the surface extension for `wsi`. */
static bool inst_up(struct nb_vkp *v, const char *wsi, char *why, size_t n)
{
    const char *ext[2] = { VK_KHR_SURFACE_EXTENSION_NAME, wsi };
    VkApplicationInfo app = {
        .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
        .pApplicationName = "conduit-viewer",
        .apiVersion = VK_API_VERSION_1_2,
    };
    VkInstanceCreateInfo ci = {
        .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
        .pApplicationInfo = &app,
        .enabledExtensionCount = 2,
        .ppEnabledExtensionNames = ext,
    };
    VkResult r;

    if (!load_global(v, why, n)) {
        return false;
    }
    /* The loader reads the layer's enable variable here. */
    nb_vkp_env();
    r = v->f.vkCreateInstance(&ci, NULL, &v->inst);
    if (r != VK_SUCCESS) {
        say(why, n, "vkCreateInstance failed", r);
        return false;
    }
    if (!load_instance(v)) {
        say(why, n, "the Vulkan instance lacks surface functions", VK_SUCCESS);
        return false;
    }
    return true;
}

/* The NVIDIA device that presents to the surface, its queue, the device. */
static bool dev_up(struct nb_vkp *v, char *why, size_t n)
{
    struct vk_fns *f = &v->f;
    VkPhysicalDevice pds[8];
    uint32_t npd = 8;
    VkResult r;

    r = f->vkEnumeratePhysicalDevices(v->inst, &npd, pds);
    if (r != VK_SUCCESS && r != VK_INCOMPLETE) {
        say(why, n, "no Vulkan devices", r);
        return false;
    }
    for (uint32_t i = 0; i < npd && !v->pd; i++) {
        VkPhysicalDeviceProperties pp;
        VkQueueFamilyProperties qf[16];
        uint32_t nqf = 16;

        f->vkGetPhysicalDeviceProperties(pds[i], &pp);
        if (!nb_vkp_gpu_ok(pp.vendorID, pp.deviceID)) {
            continue;
        }
        f->vkGetPhysicalDeviceQueueFamilyProperties(pds[i], &nqf, qf);
        for (uint32_t k = 0; k < nqf; k++) {
            VkBool32 pres = VK_FALSE;

            if (!(qf[k].queueFlags & (VK_QUEUE_GRAPHICS_BIT |
                                      VK_QUEUE_TRANSFER_BIT))) {
                continue;
            }
            if (f->vkGetPhysicalDeviceSurfaceSupportKHR(pds[i], k, v->surf,
                                                        &pres) == VK_SUCCESS &&
                pres) {
                v->pd = pds[i];
                v->qf = k;
                snprintf(v->name, sizeof(v->name), "%s", pp.deviceName);
                break;
            }
        }
    }
    if (!v->pd) {
        say(why, n, "no NVIDIA RTX 40/50 Vulkan device presents to this "
            "window", VK_SUCCESS);
        return false;
    }
    {
        VkExtensionProperties ex[512];
        uint32_t nex = 512;
        const char *want[8];
        uint32_t nw = 0;
        float prio = 1.0f;
        VkDeviceQueueGlobalPriorityCreateInfoKHR gp = {
            .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_GLOBAL_PRIORITY_CREATE_INFO_KHR,
            .globalPriority = VK_QUEUE_GLOBAL_PRIORITY_LOW_KHR,
        };
        VkDeviceQueueCreateInfo qci = {
            .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
            .queueFamilyIndex = v->qf, .queueCount = 1,
            .pQueuePriorities = &prio,
        };
        VkDeviceCreateInfo dci = {
            .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
            .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci,
        };
        static const char *const need[] = {
            VK_KHR_SWAPCHAIN_EXTENSION_NAME,
            VK_KHR_EXTERNAL_MEMORY_FD_EXTENSION_NAME,
            VK_EXT_EXTERNAL_MEMORY_DMA_BUF_EXTENSION_NAME,
            VK_EXT_IMAGE_DRM_FORMAT_MODIFIER_EXTENSION_NAME,
        };

        r = f->vkEnumerateDeviceExtensionProperties(v->pd, NULL, &nex, ex);
        if (r != VK_SUCCESS && r != VK_INCOMPLETE) {
            say(why, n, "cannot list the device's extensions", r);
            return false;
        }
        for (size_t i = 0; i < sizeof(need) / sizeof(need[0]); i++) {
            if (!has_dev_ext(v, ex, nex, need[i])) {
                snprintf(why, n, "the Vulkan device lacks %s", need[i]);
                return false;
            }
            want[nw++] = need[i];
        }
        if (has_dev_ext(v, ex, nex, VK_EXT_QUEUE_FAMILY_FOREIGN_EXTENSION_NAME)) {
            want[nw++] = VK_EXT_QUEUE_FAMILY_FOREIGN_EXTENSION_NAME;
            v->foreign = true;
        }
        if (has_dev_ext(v, ex, nex,
                        VK_KHR_EXTERNAL_SEMAPHORE_FD_EXTENSION_NAME)) {
            want[nw++] = VK_KHR_EXTERNAL_SEMAPHORE_FD_EXTENSION_NAME;
            v->sync_fd = true;
        }
        /*
         * LOW queue priority: the copy shares the GPU with the guest, and
         * must wait for the guest's channels rather than preempt them.  LOW
         * needs no privilege.
         */
        if (has_dev_ext(v, ex, nex, VK_KHR_GLOBAL_PRIORITY_EXTENSION_NAME)) {
            want[nw++] = VK_KHR_GLOBAL_PRIORITY_EXTENSION_NAME;
            v->low_prio = true;
        } else if (has_dev_ext(v, ex, nex,
                               VK_EXT_GLOBAL_PRIORITY_EXTENSION_NAME)) {
            want[nw++] = VK_EXT_GLOBAL_PRIORITY_EXTENSION_NAME;
            v->low_prio = true;
        }
        if (v->low_prio) {
            qci.pNext = &gp;
        }
        dci.enabledExtensionCount = nw;
        dci.ppEnabledExtensionNames = want;
        r = f->vkCreateDevice(v->pd, &dci, NULL, &v->dev);
        if (r != VK_SUCCESS && v->low_prio) {
            /* Without the priority rather than not at all. */
            qci.pNext = NULL;
            dci.enabledExtensionCount = nw - 1;
            v->low_prio = false;
            r = f->vkCreateDevice(v->pd, &dci, NULL, &v->dev);
        }
        if (r != VK_SUCCESS) {
            say(why, n, "vkCreateDevice failed", r);
            return false;
        }
    }
    if (!load_device(v)) {
        say(why, n, "the Vulkan device lacks functions", VK_SUCCESS);
        return false;
    }
    f->vkGetDeviceQueue(v->dev, v->qf, 0, &v->q);
    f->vkGetPhysicalDeviceMemoryProperties(v->pd, &v->mem);
    v->sync_fd = v->sync_fd && f->vkImportSemaphoreFdKHR;
    {
        VkCommandPoolCreateInfo pci = {
            .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
            .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
            .queueFamilyIndex = v->qf,
        };
        VkCommandBufferAllocateInfo ai = {
            .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
            .commandBufferCount = 1,
            .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
        };
        VkFenceCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
        VkSemaphoreCreateInfo sci = {
            .sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO,
        };

        if (f->vkCreateCommandPool(v->dev, &pci, NULL, &v->pool) != VK_SUCCESS) {
            say(why, n, "vkCreateCommandPool failed", VK_SUCCESS);
            return false;
        }
        ai.commandPool = v->pool;
        if (f->vkAllocateCommandBuffers(v->dev, &ai, &v->cb) != VK_SUCCESS ||
            f->vkCreateFence(v->dev, &fci, NULL, &v->fence) != VK_SUCCESS ||
            f->vkCreateSemaphore(v->dev, &sci, NULL, &v->acq) != VK_SUCCESS ||
            f->vkCreateSemaphore(v->dev, &sci, NULL, &v->guest_ready) !=
                VK_SUCCESS) {
            say(why, n, "cannot create the presenter's objects", VK_SUCCESS);
            return false;
        }
    }
    return true;
}

static void sc_down(struct nb_vkp *v)
{
    struct vk_fns *f = &v->f;

    for (uint32_t i = 0; i < NB_VKP_IMAGES; i++) {
        if (v->done[i]) {
            f->vkDestroySemaphore(v->dev, v->done[i], NULL);
            v->done[i] = VK_NULL_HANDLE;
        }
    }
    if (v->sc) {
        f->vkDestroySwapchainKHR(v->dev, v->sc, NULL);
        v->sc = VK_NULL_HANDLE;
    }
    v->n_img = 0;
}

/* The swapchain at w x h in present mode `mode` (NB_VKP_*). */
static int sc_up(struct nb_vkp *v, uint32_t w, uint32_t h, int mode)
{
    struct vk_fns *f = &v->f;
    VkSurfaceCapabilitiesKHR caps;
    VkSurfaceFormatKHR fmts[64];
    VkPresentModeKHR pm[16];
    int have[16];
    uint32_t nf = 64, npm = 16;
    VkSwapchainKHR old = v->sc;
    VkSwapchainCreateInfoKHR ci = {
        .sType = VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR,
        .imageArrayLayers = 1,
        .imageUsage = VK_IMAGE_USAGE_TRANSFER_DST_BIT,
        .imageSharingMode = VK_SHARING_MODE_EXCLUSIVE,
        .preTransform = VK_SURFACE_TRANSFORM_IDENTITY_BIT_KHR,
        .compositeAlpha = VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR,
        .clipped = VK_TRUE,
    };
    VkSemaphoreCreateInfo sci = { .sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO };

    f->vkDeviceWaitIdle(v->dev);
    if (f->vkGetPhysicalDeviceSurfaceCapabilitiesKHR(v->pd, v->surf, &caps) !=
            VK_SUCCESS ||
        !(caps.supportedUsageFlags & VK_IMAGE_USAGE_TRANSFER_DST_BIT)) {
        return -ENOTSUP;
    }
    if (f->vkGetPhysicalDeviceSurfaceFormatsKHR(v->pd, v->surf, &nf, fmts) <
            VK_SUCCESS ||
        f->vkGetPhysicalDeviceSurfacePresentModesKHR(v->pd, v->surf, &npm,
                                                     pm) < VK_SUCCESS) {
        return -EIO;
    }
    ci.imageFormat = VK_FORMAT_UNDEFINED;
    for (uint32_t i = 0; i < nf; i++) {
        /* UNORM: the blit copies the guest's values as they are. */
        if (fmts[i].format == VK_FORMAT_B8G8R8A8_UNORM ||
            (ci.imageFormat == VK_FORMAT_UNDEFINED &&
             fmts[i].format == VK_FORMAT_R8G8B8A8_UNORM)) {
            ci.imageFormat = fmts[i].format;
            ci.imageColorSpace = fmts[i].colorSpace;
        }
    }
    if (ci.imageFormat == VK_FORMAT_UNDEFINED) {
        return -ENOTSUP;
    }
    for (uint32_t i = 0; i < npm && i < 16; i++) {
        have[i] = (int)pm[i];
    }
    ci.presentMode = (VkPresentModeKHR)nb_vkp_pick_mode(have, npm, mode);
    ci.surface = v->surf;
    ci.minImageCount = caps.minImageCount < 3 ? 3 : caps.minImageCount;
    if (caps.maxImageCount && ci.minImageCount > caps.maxImageCount) {
        ci.minImageCount = caps.maxImageCount;
    }
    if (caps.currentExtent.width != UINT32_MAX) {
        ci.imageExtent = caps.currentExtent;   /* X11: the window decides */
    } else {
        ci.imageExtent.width = w;
        ci.imageExtent.height = h;
    }
    ci.oldSwapchain = old;
    if (f->vkCreateSwapchainKHR(v->dev, &ci, NULL, &v->sc) != VK_SUCCESS) {
        v->sc = old;
        return -EIO;
    }
    v->sc_fmt = ci.imageFormat;
    v->sc_ext = ci.imageExtent;
    v->req_w = w;
    v->req_h = h;
    v->sc_mode = mode;
    if (old) {
        f->vkDestroySwapchainKHR(v->dev, old, NULL);
    }
    memset(v->bars, 0, sizeof(v->bars));
    v->n_img = NB_VKP_IMAGES;
    if (f->vkGetSwapchainImagesKHR(v->dev, v->sc, &v->n_img, v->img) <
        VK_SUCCESS) {
        sc_down(v);
        return -EIO;
    }
    for (uint32_t i = 0; i < v->n_img; i++) {
        if (!v->done[i] &&
            f->vkCreateSemaphore(v->dev, &sci, NULL, &v->done[i]) != VK_SUCCESS) {
            sc_down(v);
            return -EIO;
        }
    }
    return 0;
}

static struct nb_vkp *vkp_finish(struct nb_vkp *v, char *why, size_t n)
{
    if (!dev_up(v, why, n)) {
        nb_vkp_close(v);
        return NULL;
    }
    return v;
}

#ifdef NB_HAVE_WAYLAND
struct nb_vkp *nb_vkp_open_wayland(void *dpy, void *surf, char *why, size_t n)
{
    struct nb_vkp *v = calloc(1, sizeof(*v));
    VkWaylandSurfaceCreateInfoKHR ci = {
        .sType = VK_STRUCTURE_TYPE_WAYLAND_SURFACE_CREATE_INFO_KHR,
        .display = dpy, .surface = surf,
    };

    if (!v) {
        return NULL;
    }
    if (!inst_up(v, VK_KHR_WAYLAND_SURFACE_EXTENSION_NAME, why, n) ||
        !(v->f.vkCreateWaylandSurfaceKHR = (PFN_vkCreateWaylandSurfaceKHR)
              v->f.vkGetInstanceProcAddr(v->inst, "vkCreateWaylandSurfaceKHR")) ||
        v->f.vkCreateWaylandSurfaceKHR(v->inst, &ci, NULL, &v->surf) !=
            VK_SUCCESS) {
        if (why && !why[0]) {
            say(why, n, "cannot make a Vulkan surface on the window",
                VK_SUCCESS);
        }
        nb_vkp_close(v);
        return NULL;
    }
    return vkp_finish(v, why, n);
}
#else
struct nb_vkp *nb_vkp_open_wayland(void *d, void *s, char *why, size_t n)
{
    (void)d; (void)s;
    snprintf(why, n, "built without Wayland");
    return NULL;
}
#endif

#ifdef NB_HAVE_X11
struct nb_vkp *nb_vkp_open_xcb(void *conn, uint32_t window, char *why,
                               size_t n)
{
    struct nb_vkp *v = calloc(1, sizeof(*v));
    VkXcbSurfaceCreateInfoKHR ci = {
        .sType = VK_STRUCTURE_TYPE_XCB_SURFACE_CREATE_INFO_KHR,
        .connection = conn, .window = window,
    };

    if (!v) {
        return NULL;
    }
    if (!inst_up(v, VK_KHR_XCB_SURFACE_EXTENSION_NAME, why, n) ||
        !(v->f.vkCreateXcbSurfaceKHR = (PFN_vkCreateXcbSurfaceKHR)
              v->f.vkGetInstanceProcAddr(v->inst, "vkCreateXcbSurfaceKHR")) ||
        v->f.vkCreateXcbSurfaceKHR(v->inst, &ci, NULL, &v->surf) !=
            VK_SUCCESS) {
        if (why && !why[0]) {
            say(why, n, "cannot make a Vulkan surface on the window",
                VK_SUCCESS);
        }
        nb_vkp_close(v);
        return NULL;
    }
    return vkp_finish(v, why, n);
}
#else
struct nb_vkp *nb_vkp_open_xcb(void *c, uint32_t w, char *why, size_t n)
{
    (void)c; (void)w;
    snprintf(why, n, "built without X11");
    return NULL;
}
#endif

void nb_vkp_forget(struct nb_vkp *v, int key)
{
    if (!v || !v->dev) {
        return;
    }
    for (int i = 0; i < NB_VKP_SRC; i++) {
        if (v->s[i].valid && v->s[i].key == key) {
            v->f.vkDeviceWaitIdle(v->dev);
            v->f.vkDestroyImage(v->dev, v->s[i].img, NULL);
            v->f.vkFreeMemory(v->dev, v->s[i].mem, NULL);
            memset(&v->s[i], 0, sizeof(v->s[i]));
        }
    }
}

void nb_vkp_close(struct nb_vkp *v)
{
    struct vk_fns *f;

    if (!v) {
        return;
    }
    f = &v->f;
    if (v->dev) {
        f->vkDeviceWaitIdle(v->dev);
        for (int i = 0; i < NB_VKP_SRC; i++) {
            nb_vkp_forget(v, v->s[i].valid ? v->s[i].key : -1);
        }
        sc_down(v);
        if (v->acq) {
            f->vkDestroySemaphore(v->dev, v->acq, NULL);
        }
        if (v->guest_ready) {
            f->vkDestroySemaphore(v->dev, v->guest_ready, NULL);
        }
        if (v->fence) {
            f->vkDestroyFence(v->dev, v->fence, NULL);
        }
        if (v->pool) {
            f->vkDestroyCommandPool(v->dev, v->pool, NULL);
        }
        f->vkDestroyDevice(v->dev, NULL);
    }
    if (v->inst) {
        if (v->surf && f->vkDestroySurfaceKHR) {
            f->vkDestroySurfaceKHR(v->inst, v->surf, NULL);
        }
        if (f->vkDestroyInstance) {
            f->vkDestroyInstance(v->inst, NULL);
        }
    }
    /* libvulkan stays loaded, as libEGL does for the GPU pass. */
    free(v);
}

const char *nb_vkp_device(const struct nb_vkp *v)
{
    return v ? v->name : "";
}

static int mem_type(const struct nb_vkp *v, uint32_t bits)
{
    for (uint32_t i = 0; i < v->mem.memoryTypeCount; i++) {
        if (bits & (1u << i)) {
            return (int)i;
        }
    }
    return -1;
}

/* The guest buffer of f as a VkImage, imported once per slot. */
static int src_get(struct nb_vkp *v, const struct nb_vkp_frame *fr)
{
    struct vk_fns *f = &v->f;
    VkFormat fmt = fourcc_fmt(fr->fourcc);
    int slot = -1;

    for (int i = 0; i < NB_VKP_SRC; i++) {
        if (v->s[i].valid && v->s[i].key == fr->key) {
            if (v->s[i].w == fr->w && v->s[i].h == fr->h &&
                v->s[i].fourcc == fr->fourcc && v->s[i].mod == fr->modifier) {
                return i;
            }
            nb_vkp_forget(v, fr->key);  /* the slot holds another buffer */
        }
    }
    for (int i = 0; i < NB_VKP_SRC && slot < 0; i++) {
        if (!v->s[i].valid) {
            slot = i;
        }
    }
    /* Implicit modifiers cannot be described to Vulkan exactly. */
    if (slot < 0 || fmt == VK_FORMAT_UNDEFINED ||
        fr->modifier == 0x00ffffffffffffffull) {
        return -1;
    }
    {
        VkSubresourceLayout pl = {
            .offset = fr->offset, .rowPitch = fr->stride,
        };
        VkImageDrmFormatModifierExplicitCreateInfoEXT mi = {
            .sType = VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT,
            .drmFormatModifier = fr->modifier,
            .drmFormatModifierPlaneCount = 1,
            .pPlaneLayouts = &pl,
        };
        VkExternalMemoryImageCreateInfo ei = {
            .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
            .pNext = &mi,
            .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
        };
        VkImageCreateInfo ci = {
            .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
            .pNext = &ei,
            .imageType = VK_IMAGE_TYPE_2D,
            .format = fmt,
            .extent = { fr->w, fr->h, 1 },
            .mipLevels = 1, .arrayLayers = 1,
            .samples = VK_SAMPLE_COUNT_1_BIT,
            .tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT,
            .usage = VK_IMAGE_USAGE_TRANSFER_SRC_BIT,
            .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
            .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
        };
        VkMemoryFdPropertiesKHR fp = {
            .sType = VK_STRUCTURE_TYPE_MEMORY_FD_PROPERTIES_KHR,
        };
        VkMemoryRequirements req;
        VkImage img;
        VkDeviceMemory mem;
        int fd, mt;

        if (f->vkCreateImage(v->dev, &ci, NULL, &img) != VK_SUCCESS) {
            return -1;
        }
        f->vkGetImageMemoryRequirements(v->dev, img, &req);
        fd = dup(fr->fd);       /* the import takes ownership on success */
        if (fd < 0 ||
            f->vkGetMemoryFdPropertiesKHR(v->dev,
                VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT, fd, &fp) !=
                VK_SUCCESS ||
            (mt = mem_type(v, req.memoryTypeBits & fp.memoryTypeBits)) < 0) {
            if (fd >= 0) {
                close(fd);
            }
            f->vkDestroyImage(v->dev, img, NULL);
            return -1;
        }
        {
            VkMemoryDedicatedAllocateInfo dai = {
                .sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
                .image = img,
            };
            VkImportMemoryFdInfoKHR ii = {
                .sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR,
                .pNext = &dai,
                .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
                .fd = fd,
            };
            VkMemoryAllocateInfo ai = {
                .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                .pNext = &ii,
                .allocationSize = req.size,
                .memoryTypeIndex = (uint32_t)mt,
            };

            if (f->vkAllocateMemory(v->dev, &ai, NULL, &mem) != VK_SUCCESS) {
                close(fd);
                f->vkDestroyImage(v->dev, img, NULL);
                return -1;
            }
        }
        if (f->vkBindImageMemory(v->dev, img, mem, 0) != VK_SUCCESS) {
            f->vkFreeMemory(v->dev, mem, NULL);
            f->vkDestroyImage(v->dev, img, NULL);
            return -1;
        }
        v->s[slot].valid = true;
        v->s[slot].key = fr->key;
        v->s[slot].w = fr->w;
        v->s[slot].h = fr->h;
        v->s[slot].fourcc = fr->fourcc;
        v->s[slot].mod = fr->modifier;
        v->s[slot].img = img;
        v->s[slot].mem = mem;
    }
    return slot;
}

/* The guest buffer's pending writes as a semaphore (implicit sync), when the
 * kernel exports them.  true: guest_ready is to be waited on. */
static bool guest_fence(struct nb_vkp *v, int dmabuf_fd)
{
#ifdef DMA_BUF_IOCTL_EXPORT_SYNC_FILE
    struct dma_buf_export_sync_file ex = { .flags = DMA_BUF_SYNC_READ, .fd = -1 };
    VkImportSemaphoreFdInfoKHR ii = {
        .sType = VK_STRUCTURE_TYPE_IMPORT_SEMAPHORE_FD_INFO_KHR,
        .semaphore = v->guest_ready,
        .flags = VK_SEMAPHORE_IMPORT_TEMPORARY_BIT,
        .handleType = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT,
    };

    if (!v->sync_fd || ioctl(dmabuf_fd, DMA_BUF_IOCTL_EXPORT_SYNC_FILE,
                             &ex) != 0 || ex.fd < 0) {
        return false;
    }
    ii.fd = ex.fd;
    if (v->f.vkImportSemaphoreFdKHR(v->dev, &ii) != VK_SUCCESS) {
        close(ex.fd);
        return false;
    }
    return true;
#else
    (void)v; (void)dmabuf_fd;
    return false;
#endif
}

static uint64_t mono_ns(void)
{
    struct timespec ts;

    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

int nb_vkp_present(struct nb_vkp *v, const struct nb_vkp_frame *fr,
                   unsigned timeout_us, unsigned *gpu_us)
{
    struct vk_fns *f = &v->f;
    uint32_t idx = 0;
    int si;
    VkResult r;
    bool wait_guest;
    uint64_t t0;

    if (!v->sc || v->sc_mode != fr->mode || v->req_w != fr->out_w ||
        v->req_h != fr->out_h) {
        int e = sc_up(v, fr->out_w, fr->out_h, fr->mode);

        if (e < 0) {
            return e;
        }
    }
    si = src_get(v, fr);
    if (si < 0) {
        return -ENOTSUP;
    }
    r = f->vkAcquireNextImageKHR(v->dev, v->sc, 0, v->acq, VK_NULL_HANDLE,
                                 &idx);
    if (r == VK_NOT_READY || r == VK_TIMEOUT) {
        return 0;               /* every image is queued: not now */
    }
    if (r == VK_ERROR_OUT_OF_DATE_KHR) {
        sc_down(v);             /* remade at the next frame */
        return 0;
    }
    if (r != VK_SUCCESS && r != VK_SUBOPTIMAL_KHR) {
        return -EIO;
    }
    t0 = mono_ns();
    wait_guest = guest_fence(v, fr->fd);
    {
        /* The bars are black already in an image presented with this
         * destination: keep its contents, clear nothing. */
        bool keep = idx < NB_VKP_IMAGES && v->bars[idx].ok &&
                    v->bars[idx].x == fr->dx && v->bars[idx].y == fr->dy &&
                    v->bars[idx].w == fr->dw && v->bars[idx].h == fr->dh;
        VkCommandBufferBeginInfo bi = {
            .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
            .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
        };
        VkImageSubresourceRange rng = {
            .aspectMask = VK_IMAGE_ASPECT_COLOR_BIT,
            .levelCount = 1, .layerCount = 1,
        };
        uint32_t ext_qf = v->foreign ? VK_QUEUE_FAMILY_FOREIGN_EXT
                                     : VK_QUEUE_FAMILY_EXTERNAL;
        VkImageMemoryBarrier pre[2] = {
            {   /* the guest's image: from its producer, for reading */
                .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                .srcAccessMask = 0,
                .dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT,
                .oldLayout = VK_IMAGE_LAYOUT_GENERAL,
                .newLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
                .srcQueueFamilyIndex = ext_qf,
                .dstQueueFamilyIndex = v->qf,
                .image = v->s[si].img,
                .subresourceRange = rng,
            },
            {   /* the swapchain image: written whole */
                .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                .srcAccessMask = 0,
                .dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
                .oldLayout = keep ? VK_IMAGE_LAYOUT_PRESENT_SRC_KHR
                                  : VK_IMAGE_LAYOUT_UNDEFINED,
                .newLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
                .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
                .image = v->img[idx],
                .subresourceRange = rng,
            },
        };
        VkImageMemoryBarrier post[2] = {
            {
                .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                .srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
                .dstAccessMask = 0,
                .oldLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                .newLayout = VK_IMAGE_LAYOUT_PRESENT_SRC_KHR,
                .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
                .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
                .image = v->img[idx],
                .subresourceRange = rng,
            },
            {   /* the guest's image back to its producer */
                .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                .srcAccessMask = VK_ACCESS_TRANSFER_READ_BIT,
                .dstAccessMask = 0,
                .oldLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
                .newLayout = VK_IMAGE_LAYOUT_GENERAL,
                .srcQueueFamilyIndex = v->qf,
                .dstQueueFamilyIndex = ext_qf,
                .image = v->s[si].img,
                .subresourceRange = rng,
            },
        };
        VkClearColorValue black = { .float32 = { 0, 0, 0, 1 } };
        VkImageBlit blit = {
            .srcSubresource = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1 },
            .dstSubresource = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1 },
        };
        int32_t sx0 = (int32_t)(fr->sx + 0.5), sy0 = (int32_t)(fr->sy + 0.5);
        int32_t sx1 = (int32_t)(fr->sx + fr->sw + 0.5);
        int32_t sy1 = (int32_t)(fr->sy + fr->sh + 0.5);
        int32_t dx1 = fr->dx + fr->dw, dy1 = fr->dy + fr->dh;

        /* Clamp to what both images have (the window may have changed size
         * since the placement was computed). */
        if (sx1 > (int32_t)fr->w) sx1 = (int32_t)fr->w;
        if (sy1 > (int32_t)fr->h) sy1 = (int32_t)fr->h;
        if (dx1 > (int32_t)v->sc_ext.width) dx1 = (int32_t)v->sc_ext.width;
        if (dy1 > (int32_t)v->sc_ext.height) dy1 = (int32_t)v->sc_ext.height;
        blit.srcOffsets[0] = (VkOffset3D){ sx0, sy0, 0 };
        blit.srcOffsets[1] = (VkOffset3D){ sx1, sy1, 1 };
        blit.dstOffsets[0] = (VkOffset3D){ fr->dx > 0 ? fr->dx : 0,
                                           fr->dy > 0 ? fr->dy : 0, 0 };
        blit.dstOffsets[1] = (VkOffset3D){ dx1, dy1, 1 };

        f->vkResetCommandBuffer(v->cb, 0);
        f->vkBeginCommandBuffer(v->cb, &bi);
        f->vkCmdPipelineBarrier(v->cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,
                                VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL, 0,
                                NULL, 2, pre);
        if (!keep) {
            f->vkCmdClearColorImage(v->cb, v->img[idx],
                                    VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                                    &black, 1, &rng);
            if (idx < NB_VKP_IMAGES) {
                v->bars[idx].ok = true;
                v->bars[idx].x = fr->dx;
                v->bars[idx].y = fr->dy;
                v->bars[idx].w = fr->dw;
                v->bars[idx].h = fr->dh;
            }
        }
        {
            VkMemoryBarrier mb = {
                .sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
                .srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
                .dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
            };

            f->vkCmdPipelineBarrier(v->cb, VK_PIPELINE_STAGE_TRANSFER_BIT,
                                    VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 1, &mb,
                                    0, NULL, 0, NULL);
        }
        if (sx1 > sx0 && sy1 > sy0 && dx1 > blit.dstOffsets[0].x &&
            dy1 > blit.dstOffsets[0].y) {
            f->vkCmdBlitImage(v->cb, v->s[si].img,
                              VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, v->img[idx],
                              VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, 1, &blit,
                              fr->nearest ? VK_FILTER_NEAREST
                                          : VK_FILTER_LINEAR);
        }
        f->vkCmdPipelineBarrier(v->cb, VK_PIPELINE_STAGE_TRANSFER_BIT,
                                VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, 0, 0,
                                NULL, 0, NULL, 2, post);
        f->vkEndCommandBuffer(v->cb);
    }
    {
        VkSemaphore waits[2] = { v->acq, v->guest_ready };
        VkPipelineStageFlags stages[2] = { VK_PIPELINE_STAGE_TRANSFER_BIT,
                                           VK_PIPELINE_STAGE_TRANSFER_BIT };
        VkSubmitInfo si_ = {
            .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
            .waitSemaphoreCount = wait_guest ? 2 : 1,
            .pWaitSemaphores = waits,
            .pWaitDstStageMask = stages,
            .commandBufferCount = 1,
            .pCommandBuffers = &v->cb,
            .signalSemaphoreCount = 1,
            .pSignalSemaphores = &v->done[idx],
        };
        VkPresentInfoKHR pi = {
            .sType = VK_STRUCTURE_TYPE_PRESENT_INFO_KHR,
            .waitSemaphoreCount = 1,
            .pWaitSemaphores = &v->done[idx],
            .swapchainCount = 1,
            .pSwapchains = &v->sc,
            .pImageIndices = &idx,
        };

        f->vkResetFences(v->dev, 1, &v->fence);
        if (f->vkQueueSubmit(v->q, 1, &si_, v->fence) != VK_SUCCESS) {
            return -EIO;
        }
        r = f->vkQueuePresentKHR(v->q, &pi);
        if (r == VK_ERROR_OUT_OF_DATE_KHR || r == VK_SUBOPTIMAL_KHR) {
            /* Shown or not, the next frame remakes the swapchain. */
            v->req_w = 0;
        } else if (r != VK_SUCCESS) {
            f->vkWaitForFences(v->dev, 1, &v->fence, VK_TRUE,
                               (uint64_t)timeout_us * 1000u);
            return -EIO;
        }
        /* The blit has read the guest's buffer once the fence signals. */
        r = f->vkWaitForFences(v->dev, 1, &v->fence, VK_TRUE,
                               (uint64_t)timeout_us * 1000u);
        if (gpu_us) {
            *gpu_us = (unsigned)((mono_ns() - t0) / 1000u);
        }
        if (r != VK_SUCCESS) {
            return r == VK_TIMEOUT ? -ETIMEDOUT : -EIO;
        }
    }
    return 1;
}

#endif /* NB_HAVE_VULKAN */
