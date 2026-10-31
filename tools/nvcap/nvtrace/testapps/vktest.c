/*
 * vktest: Vulkan compute smoke test.
 *
 * Loads vulkan-1.dll at run time, creates a Vulkan 1.2 instance, picks the
 * NVIDIA physical device (vendor 0x10DE), creates a device with one compute
 * queue and timelineSemaphore enabled, then:
 *   submit 1: copy staging(host) -> input(device-local), dispatch a compute
 *             shader out[i] = in[i]*2+1 into output(device-local),
 *             signal timeline value 1
 *   submit 2: GPU-waits timeline value 1, copies output -> readback(host),
 *             signals timeline value 2
 * The host waits for value 2 with vkWaitSemaphores (5 s timeout) and checks
 * every element.
 * Steady state (default, all deterministic):
 *   compute loop: one reusable command buffer (copy out -> in, dispatch)
 *     submitted --iters times (default 200, timeline value per submit), so
 *     out = f^(iters+1)(src) with f(x) = 2x+1, checked on the host
 *   allocation sweep: device-local + host-visible buffers of 4 KiB .. 16 MiB,
 *     pattern up through the device-local one and back, checked
 *   present loop: Win32 surface + FIFO swapchain, --frames (default 300)
 *     frames cleared to a color ramp and presented (skipped with a warning
 *     when VK_KHR_win32_surface / VK_KHR_swapchain or present support on the
 *     queue family are missing)
 * Options: --async-compute  use a compute-only queue family if there is one
 *                           (no present loop then)
 *          --frames N, --iters N, --no-present
 *          --quick          one dispatch only (the old smoke test)
 * Prints step-by-step progress and a final "PASS" or "FAIL: <reason>" line.
 */
#include "common.h"
#include <stdint.h>

#define VK_NO_PROTOTYPES
#include <vulkan/vulkan_core.h>

#include "vktest_comp.h" /* vktest_comp_spv[], generated from vktest.comp */

/* VK_KHR_win32_surface (vulkan_win32.h is not vendored here) */
#define VK_KHR_WIN32_SURFACE_EXTENSION_NAME "VK_KHR_win32_surface"
#define VK_STRUCTURE_TYPE_WIN32_SURFACE_CREATE_INFO_KHR_ ((VkStructureType)1000009000)
typedef struct {
    VkStructureType sType;
    const void *pNext;
    VkFlags flags;
    HINSTANCE hinstance;
    HWND hwnd;
} Win32SurfaceCreateInfo;
typedef VkResult(VKAPI_PTR *PFN_CreateWin32Surface)(VkInstance, const Win32SurfaceCreateInfo *,
                                                    const VkAllocationCallbacks *, VkSurfaceKHR *);

static int frames = 300, iters = 200;

#define COUNT 65536u
#define BUF_BYTES ((VkDeviceSize)COUNT * sizeof(uint32_t))

static const char *vk_res(VkResult r)
{
    static char buf[48];
    switch (r) {
    case VK_SUCCESS: return "VK_SUCCESS";
    case VK_NOT_READY: return "VK_NOT_READY";
    case VK_TIMEOUT: return "VK_TIMEOUT";
    case VK_ERROR_OUT_OF_HOST_MEMORY: return "VK_ERROR_OUT_OF_HOST_MEMORY";
    case VK_ERROR_OUT_OF_DEVICE_MEMORY: return "VK_ERROR_OUT_OF_DEVICE_MEMORY";
    case VK_ERROR_INITIALIZATION_FAILED: return "VK_ERROR_INITIALIZATION_FAILED";
    case VK_ERROR_DEVICE_LOST: return "VK_ERROR_DEVICE_LOST";
    case VK_ERROR_EXTENSION_NOT_PRESENT: return "VK_ERROR_EXTENSION_NOT_PRESENT";
    case VK_ERROR_FEATURE_NOT_PRESENT: return "VK_ERROR_FEATURE_NOT_PRESENT";
    case VK_ERROR_INCOMPATIBLE_DRIVER: return "VK_ERROR_INCOMPATIBLE_DRIVER";
    default:
        snprintf(buf, sizeof(buf), "VkResult %d", (int)r);
        return buf;
    }
}

#define VK_CHECK(call)                                                   \
    do {                                                                 \
        VkResult r_ = (call);                                            \
        if (r_ != VK_SUCCESS)                                            \
            return fail("%s failed: %s", #call, vk_res(r_));             \
    } while (0)

/* Function pointers, loaded in load_*_funcs(). */
#define VK_GLOBAL_FUNCS(X) \
    X(vkCreateInstance) X(vkEnumerateInstanceVersion) X(vkEnumerateInstanceExtensionProperties)
#define VK_INSTANCE_FUNCS(X)                                                        \
    X(vkDestroyInstance) X(vkEnumeratePhysicalDevices) X(vkGetPhysicalDeviceProperties2) \
    X(vkGetPhysicalDeviceQueueFamilyProperties) X(vkGetPhysicalDeviceMemoryProperties)  \
    X(vkGetPhysicalDeviceFeatures2) X(vkCreateDevice) X(vkGetDeviceProcAddr)        \
    X(vkEnumerateDeviceExtensionProperties)
#define VK_DEVICE_FUNCS(X)                                                          \
    X(vkDestroyDevice) X(vkGetDeviceQueue) X(vkDeviceWaitIdle) X(vkCreateBuffer)     \
    X(vkDestroyBuffer) X(vkGetBufferMemoryRequirements) X(vkAllocateMemory)          \
    X(vkFreeMemory) X(vkBindBufferMemory) X(vkMapMemory) X(vkUnmapMemory)            \
    X(vkCreateShaderModule) X(vkDestroyShaderModule) X(vkCreateDescriptorSetLayout)  \
    X(vkDestroyDescriptorSetLayout) X(vkCreatePipelineLayout)                        \
    X(vkDestroyPipelineLayout) X(vkCreateComputePipelines) X(vkDestroyPipeline)      \
    X(vkCreateDescriptorPool) X(vkDestroyDescriptorPool) X(vkAllocateDescriptorSets) \
    X(vkUpdateDescriptorSets) X(vkCreateCommandPool) X(vkDestroyCommandPool)         \
    X(vkAllocateCommandBuffers) X(vkBeginCommandBuffer) X(vkEndCommandBuffer)        \
    X(vkCmdBindPipeline) X(vkCmdBindDescriptorSets) X(vkCmdPushConstants)            \
    X(vkCmdDispatch) X(vkCmdPipelineBarrier) X(vkCmdCopyBuffer) X(vkQueueSubmit)     \
    X(vkCreateSemaphore) X(vkDestroySemaphore) X(vkWaitSemaphores)                   \
    X(vkGetSemaphoreCounterValue) X(vkCreateFence) X(vkDestroyFence)                 \
    X(vkWaitForFences) X(vkResetFences) X(vkResetCommandBuffer)                      \
    X(vkCmdClearColorImage) X(vkFreeCommandBuffers)
/* WSI, loaded only when the extensions are enabled */
#define VK_WSI_INSTANCE_FUNCS(X)                                                    \
    X(vkDestroySurfaceKHR) X(vkGetPhysicalDeviceSurfaceSupportKHR)                   \
    X(vkGetPhysicalDeviceSurfaceCapabilitiesKHR) X(vkGetPhysicalDeviceSurfaceFormatsKHR)
#define VK_WSI_DEVICE_FUNCS(X)                                                      \
    X(vkCreateSwapchainKHR) X(vkDestroySwapchainKHR) X(vkGetSwapchainImagesKHR)      \
    X(vkAcquireNextImageKHR) X(vkQueuePresentKHR)

#define DECLARE(name) static PFN_##name name;
VK_GLOBAL_FUNCS(DECLARE)
VK_INSTANCE_FUNCS(DECLARE)
VK_DEVICE_FUNCS(DECLARE)
VK_WSI_INSTANCE_FUNCS(DECLARE)
VK_WSI_DEVICE_FUNCS(DECLARE)
static PFN_vkGetInstanceProcAddr vkGetInstanceProcAddr;
static PFN_CreateWin32Surface vkCreateWin32SurfaceKHR_;

static const char *missing_fn;

static int load_global_funcs(void)
{
#define LOAD_G(name)                                                              \
    name = (PFN_##name)vkGetInstanceProcAddr(VK_NULL_HANDLE, #name);              \
    if (!name && strcmp(#name, "vkEnumerateInstanceVersion") &&                   \
        strcmp(#name, "vkEnumerateInstanceExtensionProperties")) { missing_fn = #name; return 0; }
    VK_GLOBAL_FUNCS(LOAD_G)
    return 1;
}

static int load_instance_funcs(VkInstance inst)
{
#define LOAD_I(name)                                                              \
    name = (PFN_##name)vkGetInstanceProcAddr(inst, #name);                        \
    if (!name) { missing_fn = #name; return 0; }
    VK_INSTANCE_FUNCS(LOAD_I)
    return 1;
}

static int load_wsi_instance_funcs(VkInstance inst)
{
    VK_WSI_INSTANCE_FUNCS(LOAD_I)
    vkCreateWin32SurfaceKHR_ = (PFN_CreateWin32Surface)vkGetInstanceProcAddr(inst, "vkCreateWin32SurfaceKHR");
    if (!vkCreateWin32SurfaceKHR_) { missing_fn = "vkCreateWin32SurfaceKHR"; return 0; }
    return 1;
}

static int load_device_funcs(VkDevice dev)
{
#define LOAD_D(name)                                                              \
    name = (PFN_##name)vkGetDeviceProcAddr(dev, #name);                           \
    if (!name) { missing_fn = #name; return 0; }
    VK_DEVICE_FUNCS(LOAD_D)
    return 1;
}

static int load_wsi_device_funcs(VkDevice dev)
{
    VK_WSI_DEVICE_FUNCS(LOAD_D)
    return 1;
}

static int has_instance_ext(const char *want)
{
    VkExtensionProperties e[256];
    uint32_t n = 256, i;
    if (!vkEnumerateInstanceExtensionProperties ||
        vkEnumerateInstanceExtensionProperties(NULL, &n, e) < 0)
        return 0;
    for (i = 0; i < n; i++)
        if (!strcmp(e[i].extensionName, want))
            return 1;
    return 0;
}

static int has_device_ext(VkPhysicalDevice pd, const char *want)
{
    static VkExtensionProperties e[512];
    uint32_t n = 512, i;
    if (vkEnumerateDeviceExtensionProperties(pd, NULL, &n, e) < 0)
        return 0;
    for (i = 0; i < n; i++)
        if (!strcmp(e[i].extensionName, want))
            return 1;
    return 0;
}

static VkPhysicalDeviceMemoryProperties mem_props;
static VkDevice device;

typedef struct {
    VkBuffer buf;
    VkDeviceMemory mem;
    void *map;
    const char *name;
} Buffer;

/* Picks a memory type with all 'want' flags, preferring one without 'avoid' flags. */
static int find_mem_type(uint32_t type_bits, VkMemoryPropertyFlags want, VkMemoryPropertyFlags avoid)
{
    uint32_t i;
    int pass_no;
    for (pass_no = 0; pass_no < 2; pass_no++) {
        for (i = 0; i < mem_props.memoryTypeCount; i++) {
            VkMemoryPropertyFlags f = mem_props.memoryTypes[i].propertyFlags;
            if (!(type_bits & (1u << i)) || (f & want) != want)
                continue;
            if (pass_no == 0 && (f & avoid))
                continue;
            return (int)i;
        }
    }
    return -1;
}

static int create_buffer_sz(Buffer *b, const char *name, VkBufferUsageFlags usage, int host_visible,
                            VkDeviceSize size, int quiet)
{
    VkBufferCreateInfo bci = {.sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO};
    VkMemoryAllocateInfo mai = {.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO};
    VkMemoryRequirements req;
    int type;

    b->name = name;
    b->map = NULL;
    bci.size = size;
    bci.usage = usage;
    bci.sharingMode = VK_SHARING_MODE_EXCLUSIVE;
    VK_CHECK(vkCreateBuffer(device, &bci, NULL, &b->buf));
    vkGetBufferMemoryRequirements(device, b->buf, &req);
    if (host_visible)
        type = find_mem_type(req.memoryTypeBits,
                             VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT,
                             0);
    else
        type = find_mem_type(req.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT,
                             VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT);
    if (type < 0)
        return fail("no suitable memory type for buffer %s (bits 0x%x)", name, req.memoryTypeBits);
    mai.allocationSize = req.size;
    mai.memoryTypeIndex = (uint32_t)type;
    VK_CHECK(vkAllocateMemory(device, &mai, NULL, &b->mem));
    VK_CHECK(vkBindBufferMemory(device, b->buf, b->mem, 0));
    if (host_visible)
        VK_CHECK(vkMapMemory(device, b->mem, 0, VK_WHOLE_SIZE, 0, &b->map));
    if (!quiet)
        logmsg("buffer %-8s: %llu bytes, memory type %d (flags 0x%x, heap %u)%s", name,
               (unsigned long long)req.size, type, mem_props.memoryTypes[type].propertyFlags,
               mem_props.memoryTypes[type].heapIndex, host_visible ? ", mapped" : "");
    return 0;
}

static int create_buffer(Buffer *b, const char *name, VkBufferUsageFlags usage, int host_visible)
{
    return create_buffer_sz(b, name, usage, host_visible, BUF_BYTES, 0);
}

static void destroy_buffer(Buffer *b)
{
    if (b->map)
        vkUnmapMemory(device, b->mem);
    vkDestroyBuffer(device, b->buf, NULL);
    vkFreeMemory(device, b->mem, NULL);
}

static void buffer_barrier(VkCommandBuffer cb, VkBuffer buf, VkAccessFlags src_access,
                           VkAccessFlags dst_access, VkPipelineStageFlags src_stage,
                           VkPipelineStageFlags dst_stage)
{
    VkBufferMemoryBarrier b = {.sType = VK_STRUCTURE_TYPE_BUFFER_MEMORY_BARRIER};
    b.srcAccessMask = src_access;
    b.dstAccessMask = dst_access;
    b.srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED;
    b.dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED;
    b.buffer = buf;
    b.offset = 0;
    b.size = VK_WHOLE_SIZE;
    vkCmdPipelineBarrier(cb, src_stage, dst_stage, 0, 0, NULL, 1, &b, 0, NULL);
}

/* ---- steady-state workloads ---- */

static VkQueue g_queue;
static VkCommandPool g_pool;           /* RESET_COMMAND_BUFFER */
static VkSemaphore g_timeline;
static uint64_t g_tl;                  /* last timeline value used */

/* Submits cb, signals the next timeline value and waits for it (5 s). */
static int submit_wait(VkCommandBuffer cb, const char *what)
{
    VkTimelineSemaphoreSubmitInfo tsi = {.sType = VK_STRUCTURE_TYPE_TIMELINE_SEMAPHORE_SUBMIT_INFO};
    VkSubmitInfo si = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO};
    VkSemaphoreWaitInfo swi = {.sType = VK_STRUCTURE_TYPE_SEMAPHORE_WAIT_INFO};
    uint64_t v = ++g_tl;
    VkResult r;

    tsi.signalSemaphoreValueCount = 1;
    tsi.pSignalSemaphoreValues = &v;
    si.pNext = &tsi;
    si.commandBufferCount = 1;
    si.pCommandBuffers = &cb;
    si.signalSemaphoreCount = 1;
    si.pSignalSemaphores = &g_timeline;
    VK_CHECK(vkQueueSubmit(g_queue, 1, &si, VK_NULL_HANDLE));
    swi.semaphoreCount = 1;
    swi.pSemaphores = &g_timeline;
    swi.pValues = &v;
    r = vkWaitSemaphores(device, &swi, (uint64_t)WAIT_TIMEOUT_MS * 1000000ull);
    if (r != VK_SUCCESS)
        return fail("%s: vkWaitSemaphores(%llu) returned %s", what, (unsigned long long)v, vk_res(r));
    return 0;
}

static int alloc_sweep(void)
{
    unsigned s;
    double t0 = now_ms();
    VkCommandBufferAllocateInfo cbai = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO};
    VkCommandBufferBeginInfo cbbi = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};
    VkCommandBuffer cb;

    cbai.commandPool = g_pool;
    cbai.level = VK_COMMAND_BUFFER_LEVEL_PRIMARY;
    cbai.commandBufferCount = 1;
    VK_CHECK(vkAllocateCommandBuffers(device, &cbai, &cb));
    cbbi.flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT;
    for (s = 0; s < ALLOC_SWEEP_N; s++) {
        VkDeviceSize size = alloc_sweep_sizes[s];
        VkBufferCopy rg = {0, 0, size};
        Buffer up, mid, down;
        uint32_t *u, *d, i, n = (uint32_t)(size / 4), bad = 0;

        if (create_buffer_sz(&up, "sweep-up", VK_BUFFER_USAGE_TRANSFER_SRC_BIT, 1, size, 1) ||
            create_buffer_sz(&mid, "sweep-dev",
                             VK_BUFFER_USAGE_TRANSFER_SRC_BIT | VK_BUFFER_USAGE_TRANSFER_DST_BIT, 0,
                             size, 1) ||
            create_buffer_sz(&down, "sweep-down", VK_BUFFER_USAGE_TRANSFER_DST_BIT, 1, size, 1))
            return 1;
        u = (uint32_t *)up.map;
        d = (uint32_t *)down.map;
        for (i = 0; i < n; i++) {
            u[i] = sweep_word(s, i);
            d[i] = ~u[i];
        }
        VK_CHECK(vkResetCommandBuffer(cb, 0));
        VK_CHECK(vkBeginCommandBuffer(cb, &cbbi));
        vkCmdCopyBuffer(cb, up.buf, mid.buf, 1, &rg);
        buffer_barrier(cb, mid.buf, VK_ACCESS_TRANSFER_WRITE_BIT, VK_ACCESS_TRANSFER_READ_BIT,
                       VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT);
        vkCmdCopyBuffer(cb, mid.buf, down.buf, 1, &rg);
        buffer_barrier(cb, down.buf, VK_ACCESS_TRANSFER_WRITE_BIT, VK_ACCESS_HOST_READ_BIT,
                       VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_HOST_BIT);
        VK_CHECK(vkEndCommandBuffer(cb));
        if (submit_wait(cb, "alloc sweep"))
            return 1;
        for (i = 0; i < n; i++)
            if (d[i] != u[i])
                bad++;
        destroy_buffer(&up);
        destroy_buffer(&mid);
        destroy_buffer(&down);
        if (bad)
            return fail("alloc sweep: %u of %u words wrong at %llu bytes", bad, n,
                        (unsigned long long)size);
        logmsg("alloc sweep: %llu bytes ok", (unsigned long long)size);
    }
    vkFreeCommandBuffers(device, g_pool, 1, &cb);
    logmsg("alloc sweep: %u sizes in %.1f ms", (unsigned)ALLOC_SWEEP_N, now_ms() - t0);
    return 0;
}

static LRESULT CALLBACK wnd_proc(HWND hwnd, UINT msg, WPARAM wp, LPARAM lp)
{
    return DefWindowProcW(hwnd, msg, wp, lp);
}

static void pump_messages(void)
{
    MSG msg;
    int n = 0;
    while (n++ < 100 && PeekMessageW(&msg, NULL, 0, 0, PM_REMOVE)) {
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

/* Win32 surface + FIFO swapchain, 'frames' clears + presents. 0 = ok or skipped. */
static int present_loop(VkInstance inst, VkPhysicalDevice pd, uint32_t qfam)
{
    WNDCLASSEXW wc;
    HWND hwnd;
    RECT rc = {0, 0, 640, 480};
    Win32SurfaceCreateInfo wsci = {VK_STRUCTURE_TYPE_WIN32_SURFACE_CREATE_INFO_KHR_, NULL, 0, NULL, NULL};
    VkSurfaceKHR surf;
    VkBool32 supported = VK_FALSE;
    VkSurfaceCapabilitiesKHR caps;
    VkSurfaceFormatKHR fmts[64];
    uint32_t nfmt = 64, nimg = 8, f, i;
    VkSwapchainCreateInfoKHR scci = {.sType = VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR};
    VkSwapchainKHR sc;
    VkImage imgs[8];
    VkSemaphoreCreateInfo sci = {.sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO};
    VkFenceCreateInfo fci = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
    VkSemaphore acq[2], done[2];
    VkFence fence[2];
    VkCommandBuffer cb[2];
    VkCommandBufferAllocateInfo cbai = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO};
    VkCommandBufferBeginInfo cbbi = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};
    VkImageSubresourceRange range = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1};
    double t0;

    logmsg("present loop: creating window and VkSurfaceKHR");
    memset(&wc, 0, sizeof(wc));
    wc.cbSize = sizeof(wc);
    wc.lpfnWndProc = wnd_proc;
    wc.hInstance = GetModuleHandleW(NULL);
    wc.hCursor = LoadCursorW(NULL, (LPCWSTR)IDC_ARROW);
    wc.lpszClassName = L"nvtrace_vktest";
    if (!RegisterClassExW(&wc))
        return fail("RegisterClassEx failed, error %lu", (unsigned long)GetLastError());
    AdjustWindowRect(&rc, WS_OVERLAPPEDWINDOW, FALSE);
    hwnd = CreateWindowExW(0, wc.lpszClassName, L"nvtrace vktest", WS_OVERLAPPEDWINDOW, CW_USEDEFAULT,
                           CW_USEDEFAULT, rc.right - rc.left, rc.bottom - rc.top, NULL, NULL,
                           wc.hInstance, NULL);
    if (!hwnd)
        return fail("CreateWindowEx failed, error %lu", (unsigned long)GetLastError());
    ShowWindow(hwnd, SW_SHOWNORMAL);
    pump_messages();
    wsci.hinstance = wc.hInstance;
    wsci.hwnd = hwnd;
    VK_CHECK(vkCreateWin32SurfaceKHR_(inst, &wsci, NULL, &surf));
    VK_CHECK(vkGetPhysicalDeviceSurfaceSupportKHR(pd, qfam, surf, &supported));
    if (!supported) {
        logmsg("WARNING: queue family %u cannot present to the window, skipping the present loop", qfam);
        vkDestroySurfaceKHR(inst, surf, NULL);
        DestroyWindow(hwnd);
        return 0;
    }
    VK_CHECK(vkGetPhysicalDeviceSurfaceCapabilitiesKHR(pd, surf, &caps));
    {
        VkResult fr = vkGetPhysicalDeviceSurfaceFormatsKHR(pd, surf, &nfmt, fmts);
        if (fr != VK_SUCCESS && fr != VK_INCOMPLETE)
            return fail("vkGetPhysicalDeviceSurfaceFormatsKHR failed: %s", vk_res(fr));
    }
    if (!nfmt)
        return fail("no surface formats");
    if (!(caps.supportedUsageFlags & VK_IMAGE_USAGE_TRANSFER_DST_BIT)) {
        logmsg("WARNING: swapchain images cannot be TRANSFER_DST, skipping the present loop");
        vkDestroySurfaceKHR(inst, surf, NULL);
        DestroyWindow(hwnd);
        return 0;
    }
    scci.surface = surf;
    scci.minImageCount = caps.minImageCount < 2 ? 2 : caps.minImageCount;
    if (caps.maxImageCount && scci.minImageCount > caps.maxImageCount)
        scci.minImageCount = caps.maxImageCount;
    scci.imageFormat = fmts[0].format;
    scci.imageColorSpace = fmts[0].colorSpace;
    scci.imageExtent = caps.currentExtent.width != UINT32_MAX ? caps.currentExtent
                                                               : (VkExtent2D){640, 480};
    scci.imageArrayLayers = 1;
    scci.imageUsage = VK_IMAGE_USAGE_TRANSFER_DST_BIT | VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT;
    scci.imageSharingMode = VK_SHARING_MODE_EXCLUSIVE;
    scci.preTransform = caps.currentTransform;
    scci.compositeAlpha = VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR;
    scci.presentMode = VK_PRESENT_MODE_FIFO_KHR; /* always supported */
    scci.clipped = VK_TRUE;
    logmsg("present loop: swapchain %ux%u format %d, %u images min, FIFO", scci.imageExtent.width,
           scci.imageExtent.height, (int)scci.imageFormat, scci.minImageCount);
    VK_CHECK(vkCreateSwapchainKHR(device, &scci, NULL, &sc));
    {
        VkResult ir = vkGetSwapchainImagesKHR(device, sc, &nimg, imgs);
        if (ir != VK_SUCCESS)
            return fail("vkGetSwapchainImagesKHR: %s (%u images, at most 8 supported here)",
                        vk_res(ir), nimg);
    }
    cbai.commandPool = g_pool;
    cbai.level = VK_COMMAND_BUFFER_LEVEL_PRIMARY;
    cbai.commandBufferCount = 2;
    VK_CHECK(vkAllocateCommandBuffers(device, &cbai, cb));
    fci.flags = VK_FENCE_CREATE_SIGNALED_BIT;
    for (i = 0; i < 2; i++) {
        VK_CHECK(vkCreateSemaphore(device, &sci, NULL, &acq[i]));
        VK_CHECK(vkCreateSemaphore(device, &sci, NULL, &done[i]));
        VK_CHECK(vkCreateFence(device, &fci, NULL, &fence[i]));
    }

    t0 = now_ms();
    for (f = 0; f < (uint32_t)frames; f++) {
        uint32_t k = f & 1, idx;
        if (now_ms() - t0 > PRESENT_BUDGET_MS) {
            logmsg("present loop: time budget of %.0f ms reached after %u of %d frames",
                   PRESENT_BUDGET_MS, f, frames);
            break;
        }
        float t = (float)(f % 60) / 59.0f;
        VkClearColorValue color = {{t, 1.0f - t, (f & 1) ? 0.5f : 0.25f, 1.0f}};
        VkImageMemoryBarrier ib = {.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER};
        VkPipelineStageFlags ws = VK_PIPELINE_STAGE_TRANSFER_BIT;
        VkSubmitInfo si = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO};
        VkPresentInfoKHR pi = {.sType = VK_STRUCTURE_TYPE_PRESENT_INFO_KHR};
        VkResult r;

        pump_messages();
        r = vkWaitForFences(device, 1, &fence[k], VK_TRUE, (uint64_t)WAIT_TIMEOUT_MS * 1000000ull);
        if (r != VK_SUCCESS)
            return fail("present loop: frame fence wait returned %s at frame %u", vk_res(r), f);
        VK_CHECK(vkResetFences(device, 1, &fence[k]));
        r = vkAcquireNextImageKHR(device, sc, (uint64_t)WAIT_TIMEOUT_MS * 1000000ull, acq[k],
                                  VK_NULL_HANDLE, &idx);
        if (r != VK_SUCCESS && r != VK_SUBOPTIMAL_KHR)
            return fail("present loop: vkAcquireNextImageKHR returned %s at frame %u", vk_res(r), f);
        VK_CHECK(vkResetCommandBuffer(cb[k], 0));
        cbbi.flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT;
        VK_CHECK(vkBeginCommandBuffer(cb[k], &cbbi));
        ib.srcQueueFamilyIndex = ib.dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED;
        ib.image = imgs[idx];
        ib.subresourceRange = range;
        ib.oldLayout = VK_IMAGE_LAYOUT_UNDEFINED;
        ib.newLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL;
        ib.dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
        vkCmdPipelineBarrier(cb[k], VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0,
                             0, NULL, 0, NULL, 1, &ib);
        vkCmdClearColorImage(cb[k], imgs[idx], VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, &color, 1, &range);
        ib.oldLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL;
        ib.newLayout = VK_IMAGE_LAYOUT_PRESENT_SRC_KHR;
        ib.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
        ib.dstAccessMask = 0;
        vkCmdPipelineBarrier(cb[k], VK_PIPELINE_STAGE_TRANSFER_BIT,
                             VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, 0, 0, NULL, 0, NULL, 1, &ib);
        VK_CHECK(vkEndCommandBuffer(cb[k]));
        si.waitSemaphoreCount = 1;
        si.pWaitSemaphores = &acq[k];
        si.pWaitDstStageMask = &ws;
        si.commandBufferCount = 1;
        si.pCommandBuffers = &cb[k];
        si.signalSemaphoreCount = 1;
        si.pSignalSemaphores = &done[k];
        VK_CHECK(vkQueueSubmit(g_queue, 1, &si, fence[k]));
        pi.waitSemaphoreCount = 1;
        pi.pWaitSemaphores = &done[k];
        pi.swapchainCount = 1;
        pi.pSwapchains = &sc;
        pi.pImageIndices = &idx;
        r = vkQueuePresentKHR(g_queue, &pi);
        if (r != VK_SUCCESS && r != VK_SUBOPTIMAL_KHR)
            return fail("present loop: vkQueuePresentKHR returned %s at frame %u", vk_res(r), f);
        if (r == VK_SUBOPTIMAL_KHR && f == 0)
            logmsg("present loop: VK_SUBOPTIMAL_KHR (window resized?), continuing");
    }
    VK_CHECK(vkWaitForFences(device, 2, fence, VK_TRUE, (uint64_t)WAIT_TIMEOUT_MS * 1000000ull));
    logmsg("present loop: %u frames in %.1f ms", f, now_ms() - t0);
    VK_CHECK(vkDeviceWaitIdle(device));
    for (i = 0; i < 2; i++) {
        vkDestroySemaphore(device, acq[i], NULL);
        vkDestroySemaphore(device, done[i], NULL);
        vkDestroyFence(device, fence[i], NULL);
    }
    vkFreeCommandBuffers(device, g_pool, 2, cb);
    vkDestroySwapchainKHR(device, sc, NULL);
    vkDestroySurfaceKHR(inst, surf, NULL);
    DestroyWindow(hwnd);
    pump_messages();
    UnregisterClassW(wc.lpszClassName, wc.hInstance);
    return 0;
}

int main(int argc, char **argv)
{
    HMODULE lib;
    uint32_t api = VK_API_VERSION_1_0, ndev = 0, i, nqf = 0, qfam = UINT32_MAX;
    VkApplicationInfo app = {.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO};
    VkInstanceCreateInfo ici = {.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO};
    VkInstance inst;
    VkPhysicalDevice pdevs[16], pdev = VK_NULL_HANDLE;
    VkPhysicalDeviceProperties2 props2 = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2};
    VkPhysicalDeviceIDProperties idprops = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ID_PROPERTIES};
    VkPhysicalDeviceVulkan12Features f12 = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES};
    VkPhysicalDeviceFeatures2 f2 = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2};
    VkQueueFamilyProperties qfp[32];
    float prio = 1.0f;
    VkDeviceQueueCreateInfo dqci = {.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO};
    VkDeviceCreateInfo dci = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO};
    VkQueue queue;
    Buffer staging, in_dev, out_dev, readback;
    VkShaderModuleCreateInfo smci = {.sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO};
    VkShaderModule sm;
    VkDescriptorSetLayoutBinding bind[2];
    VkDescriptorSetLayoutCreateInfo dslci = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO};
    VkDescriptorSetLayout dsl;
    VkPushConstantRange pcr = {VK_SHADER_STAGE_COMPUTE_BIT, 0, sizeof(uint32_t)};
    VkPipelineLayoutCreateInfo plci = {.sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO};
    VkPipelineLayout pl;
    VkComputePipelineCreateInfo cpci = {.sType = VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO};
    VkPipeline pipe;
    VkDescriptorPoolSize dps = {VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 2};
    VkDescriptorPoolCreateInfo dpci = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO};
    VkDescriptorPool dpool;
    VkDescriptorSetAllocateInfo dsai = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO};
    VkDescriptorSet ds;
    VkDescriptorBufferInfo dbi[2];
    VkWriteDescriptorSet wds[2];
    VkCommandPoolCreateInfo cpoolci = {.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO};
    VkCommandPool cpool;
    VkCommandBufferAllocateInfo cbai = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO};
    VkCommandBuffer cbs[2];
    VkCommandBufferBeginInfo cbbi = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};
    VkBufferCopy region = {0, 0, BUF_BYTES};
    VkSemaphoreTypeCreateInfo stci = {.sType = VK_STRUCTURE_TYPE_SEMAPHORE_TYPE_CREATE_INFO};
    VkSemaphoreCreateInfo sci = {.sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO};
    VkSemaphore timeline;
    uint64_t sig1 = 1, sig2 = 2, wait1 = 1, wait_value = 2, counter = 0;
    VkTimelineSemaphoreSubmitInfo tsi[2];
    VkSubmitInfo si[2];
    VkPipelineStageFlags wait_stage = VK_PIPELINE_STAGE_TRANSFER_BIT;
    VkSemaphoreWaitInfo swi = {.sType = VK_STRUCTURE_TYPE_SEMAPHORE_WAIT_INFO};
    VkResult r;
    uint32_t count = COUNT, bad = 0, *src, *dst;
    int async_compute = has_arg(argc, argv, "--async-compute");
    int quick = has_arg(argc, argv, "--quick");
    int want_present = !has_arg(argc, argv, "--no-present") && !async_compute && !quick;
    int wsi = 0, it;
    const char *iexts[2] = {VK_KHR_SURFACE_EXTENSION_NAME, VK_KHR_WIN32_SURFACE_EXTENSION_NAME};
    const char *dexts[1] = {VK_KHR_SWAPCHAIN_EXTENSION_NAME};
    VkCommandBuffer cbloop, cbdl;
    VkCommandPoolCreateInfo rpoolci = {.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO};
    double t0;
    char name[VK_MAX_PHYSICAL_DEVICE_NAME_SIZE + 1];

    common_init("vktest");
    frames = arg_int(argc, argv, "--frames", 300, 1, 100000);
    iters = arg_int(argc, argv, "--iters", quick ? 0 : 200, 0, 1000000);
    logmsg("workload: %d loop dispatches, %s, %s", iters, quick ? "no sweep" : "alloc sweep",
           want_present ? "present loop" : "no present loop");

    logmsg("loading vulkan-1.dll");
    lib = LoadLibraryA("vulkan-1.dll");
    if (!lib)
        return fail("LoadLibrary(vulkan-1.dll) failed, error %lu (no Vulkan loader installed?)",
                    (unsigned long)GetLastError());
    vkGetInstanceProcAddr = (PFN_vkGetInstanceProcAddr)load_sym(lib, "vkGetInstanceProcAddr");
    if (!vkGetInstanceProcAddr)
        return fail("vulkan-1.dll does not export vkGetInstanceProcAddr");
    if (!load_global_funcs())
        return fail("vkGetInstanceProcAddr(NULL, %s) returned NULL", missing_fn);
    if (vkEnumerateInstanceVersion)
        VK_CHECK(vkEnumerateInstanceVersion(&api));
    logmsg("loader instance version %u.%u.%u", VK_API_VERSION_MAJOR(api), VK_API_VERSION_MINOR(api),
           VK_API_VERSION_PATCH(api));
    if (api < VK_API_VERSION_1_2)
        return fail("Vulkan loader is older than 1.2");

    wsi = want_present && has_instance_ext(VK_KHR_SURFACE_EXTENSION_NAME) &&
          has_instance_ext(VK_KHR_WIN32_SURFACE_EXTENSION_NAME);
    if (want_present && !wsi)
        logmsg("WARNING: VK_KHR_win32_surface not available, no present loop");
    logmsg("vkCreateInstance (apiVersion 1.2, no layers, %s)",
           wsi ? "VK_KHR_surface + VK_KHR_win32_surface" : "no extensions");
    app.pApplicationName = "nvtrace-vktest";
    app.applicationVersion = 1;
    app.pEngineName = "none";
    app.apiVersion = VK_API_VERSION_1_2;
    ici.pApplicationInfo = &app;
    if (wsi) {
        ici.enabledExtensionCount = 2;
        ici.ppEnabledExtensionNames = iexts;
    }
    VK_CHECK(vkCreateInstance(&ici, NULL, &inst));
    if (!load_instance_funcs(inst))
        return fail("vkGetInstanceProcAddr(instance, %s) returned NULL", missing_fn);
    if (wsi && !load_wsi_instance_funcs(inst))
        return fail("vkGetInstanceProcAddr(instance, %s) returned NULL", missing_fn);

    VK_CHECK(vkEnumeratePhysicalDevices(inst, &ndev, NULL));
    if (ndev > 16)
        ndev = 16;
    r = vkEnumeratePhysicalDevices(inst, &ndev, pdevs);
    if (r != VK_SUCCESS && r != VK_INCOMPLETE)
        return fail("vkEnumeratePhysicalDevices failed: %s", vk_res(r));
    logmsg("%u physical device(s)", ndev);
    if (!ndev)
        return fail("no Vulkan physical devices");
    for (i = 0; i < ndev; i++) {
        props2.pNext = &idprops;
        vkGetPhysicalDeviceProperties2(pdevs[i], &props2);
        memcpy(name, props2.properties.deviceName, VK_MAX_PHYSICAL_DEVICE_NAME_SIZE);
        name[VK_MAX_PHYSICAL_DEVICE_NAME_SIZE] = 0;
        logmsg("device %u: %s, vendor 0x%04x device 0x%04x, api %u.%u.%u, driver 0x%08x", i, name,
               props2.properties.vendorID, props2.properties.deviceID,
               VK_API_VERSION_MAJOR(props2.properties.apiVersion),
               VK_API_VERSION_MINOR(props2.properties.apiVersion),
               VK_API_VERSION_PATCH(props2.properties.apiVersion), props2.properties.driverVersion);
        if (idprops.deviceLUIDValid) {
            LUID l;
            memcpy(&l, idprops.deviceLUID, sizeof(l));
            logmsg("device %u: LUID %08lx:%08lx node mask 0x%x", i, (unsigned long)l.HighPart,
                   (unsigned long)l.LowPart, idprops.deviceNodeMask);
        }
        if (pdev == VK_NULL_HANDLE && props2.properties.vendorID == NV_VENDOR_ID &&
            props2.properties.apiVersion >= VK_API_VERSION_1_2)
            pdev = pdevs[i];
    }
    if (pdev == VK_NULL_HANDLE) {
        logmsg("WARNING: no NVIDIA (0x10DE) Vulkan 1.2 device, falling back to device 0");
        pdev = pdevs[0];
    }
    props2.pNext = NULL;
    vkGetPhysicalDeviceProperties2(pdev, &props2);
    if (props2.properties.apiVersion < VK_API_VERSION_1_2)
        return fail("selected device only supports Vulkan %u.%u",
                    VK_API_VERSION_MAJOR(props2.properties.apiVersion),
                    VK_API_VERSION_MINOR(props2.properties.apiVersion));
    memcpy(name, props2.properties.deviceName, VK_MAX_PHYSICAL_DEVICE_NAME_SIZE);
    logmsg("using %s", name);

    f2.pNext = &f12;
    vkGetPhysicalDeviceFeatures2(pdev, &f2);
    if (!f12.timelineSemaphore)
        return fail("device does not support timelineSemaphore");
    vkGetPhysicalDeviceMemoryProperties(pdev, &mem_props);

    vkGetPhysicalDeviceQueueFamilyProperties(pdev, &nqf, NULL);
    if (nqf > 32)
        nqf = 32;
    vkGetPhysicalDeviceQueueFamilyProperties(pdev, &nqf, qfp);
    for (i = 0; i < nqf; i++) {
        VkQueueFlags fl = qfp[i].queueFlags;
        logmsg("queue family %u: %u queue(s), flags 0x%x%s%s%s", i, qfp[i].queueCount, fl,
               (fl & VK_QUEUE_GRAPHICS_BIT) ? " graphics" : "",
               (fl & VK_QUEUE_COMPUTE_BIT) ? " compute" : "",
               (fl & VK_QUEUE_TRANSFER_BIT) ? " transfer" : "");
    }
    if (async_compute)
        for (i = 0; i < nqf && qfam == UINT32_MAX; i++)
            if ((qfp[i].queueFlags & VK_QUEUE_COMPUTE_BIT) && !(qfp[i].queueFlags & VK_QUEUE_GRAPHICS_BIT))
                qfam = i;
    for (i = 0; i < nqf && qfam == UINT32_MAX; i++)
        if (qfp[i].queueFlags & VK_QUEUE_COMPUTE_BIT)
            qfam = i;
    if (qfam == UINT32_MAX)
        return fail("no queue family with compute support");
    logmsg("using queue family %u%s", qfam, async_compute ? " (--async-compute)" : "");

    if (wsi && !has_device_ext(pdev, VK_KHR_SWAPCHAIN_EXTENSION_NAME)) {
        logmsg("WARNING: VK_KHR_swapchain not available, no present loop");
        wsi = 0;
    }
    logmsg("vkCreateDevice (1 queue, timelineSemaphore%s)", wsi ? ", VK_KHR_swapchain" : "");
    dqci.queueFamilyIndex = qfam;
    dqci.queueCount = 1;
    dqci.pQueuePriorities = &prio;
    memset(&f12, 0, sizeof(f12));
    f12.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES;
    f12.timelineSemaphore = VK_TRUE;
    dci.pNext = &f12;
    dci.queueCreateInfoCount = 1;
    dci.pQueueCreateInfos = &dqci;
    if (wsi) {
        dci.enabledExtensionCount = 1;
        dci.ppEnabledExtensionNames = dexts;
    }
    VK_CHECK(vkCreateDevice(pdev, &dci, NULL, &device));
    if (!load_device_funcs(device))
        return fail("vkGetDeviceProcAddr(%s) returned NULL", missing_fn);
    if (wsi && !load_wsi_device_funcs(device))
        return fail("vkGetDeviceProcAddr(%s) returned NULL", missing_fn);
    vkGetDeviceQueue(device, qfam, 0, &queue);

    logmsg("creating buffers (%u uints each)", COUNT);
    if (create_buffer(&staging, "staging", VK_BUFFER_USAGE_TRANSFER_SRC_BIT, 1) ||
        create_buffer(&in_dev, "input",
                      VK_BUFFER_USAGE_STORAGE_BUFFER_BIT | VK_BUFFER_USAGE_TRANSFER_DST_BIT, 0) ||
        create_buffer(&out_dev, "output",
                      VK_BUFFER_USAGE_STORAGE_BUFFER_BIT | VK_BUFFER_USAGE_TRANSFER_SRC_BIT, 0) ||
        create_buffer(&readback, "readback", VK_BUFFER_USAGE_TRANSFER_DST_BIT, 1))
        return 1;
    src = (uint32_t *)staging.map;
    dst = (uint32_t *)readback.map;
    for (i = 0; i < COUNT; i++) {
        src[i] = i ^ 0x5A5A0000u;
        dst[i] = 0xDEADBEEFu;
    }

    logmsg("creating compute pipeline (%u bytes SPIR-V)", (unsigned)sizeof(vktest_comp_spv));
    smci.codeSize = sizeof(vktest_comp_spv);
    smci.pCode = vktest_comp_spv;
    VK_CHECK(vkCreateShaderModule(device, &smci, NULL, &sm));
    for (i = 0; i < 2; i++) {
        bind[i].binding = i;
        bind[i].descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER;
        bind[i].descriptorCount = 1;
        bind[i].stageFlags = VK_SHADER_STAGE_COMPUTE_BIT;
        bind[i].pImmutableSamplers = NULL;
    }
    dslci.bindingCount = 2;
    dslci.pBindings = bind;
    VK_CHECK(vkCreateDescriptorSetLayout(device, &dslci, NULL, &dsl));
    plci.setLayoutCount = 1;
    plci.pSetLayouts = &dsl;
    plci.pushConstantRangeCount = 1;
    plci.pPushConstantRanges = &pcr;
    VK_CHECK(vkCreatePipelineLayout(device, &plci, NULL, &pl));
    cpci.stage.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO;
    cpci.stage.stage = VK_SHADER_STAGE_COMPUTE_BIT;
    cpci.stage.module = sm;
    cpci.stage.pName = "main";
    cpci.layout = pl;
    cpci.basePipelineIndex = -1;
    VK_CHECK(vkCreateComputePipelines(device, VK_NULL_HANDLE, 1, &cpci, NULL, &pipe));

    dpci.maxSets = 1;
    dpci.poolSizeCount = 1;
    dpci.pPoolSizes = &dps;
    VK_CHECK(vkCreateDescriptorPool(device, &dpci, NULL, &dpool));
    dsai.descriptorPool = dpool;
    dsai.descriptorSetCount = 1;
    dsai.pSetLayouts = &dsl;
    VK_CHECK(vkAllocateDescriptorSets(device, &dsai, &ds));
    dbi[0].buffer = in_dev.buf;
    dbi[1].buffer = out_dev.buf;
    for (i = 0; i < 2; i++) {
        dbi[i].offset = 0;
        dbi[i].range = VK_WHOLE_SIZE;
        memset(&wds[i], 0, sizeof(wds[i]));
        wds[i].sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET;
        wds[i].dstSet = ds;
        wds[i].dstBinding = i;
        wds[i].descriptorCount = 1;
        wds[i].descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER;
        wds[i].pBufferInfo = &dbi[i];
    }
    vkUpdateDescriptorSets(device, 2, wds, 0, NULL);

    logmsg("recording command buffers");
    cpoolci.queueFamilyIndex = qfam;
    VK_CHECK(vkCreateCommandPool(device, &cpoolci, NULL, &cpool));
    cbai.commandPool = cpool;
    cbai.level = VK_COMMAND_BUFFER_LEVEL_PRIMARY;
    cbai.commandBufferCount = 2;
    VK_CHECK(vkAllocateCommandBuffers(device, &cbai, cbs));
    cbai.commandBufferCount = 1;
    VK_CHECK(vkAllocateCommandBuffers(device, &cbai, &cbloop));
    VK_CHECK(vkAllocateCommandBuffers(device, &cbai, &cbdl));
    cbbi.flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT;

    /* cb0: upload input, run the shader */
    VK_CHECK(vkBeginCommandBuffer(cbs[0], &cbbi));
    vkCmdCopyBuffer(cbs[0], staging.buf, in_dev.buf, 1, &region);
    buffer_barrier(cbs[0], in_dev.buf, VK_ACCESS_TRANSFER_WRITE_BIT, VK_ACCESS_SHADER_READ_BIT,
                   VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT);
    vkCmdBindPipeline(cbs[0], VK_PIPELINE_BIND_POINT_COMPUTE, pipe);
    vkCmdBindDescriptorSets(cbs[0], VK_PIPELINE_BIND_POINT_COMPUTE, pl, 0, 1, &ds, 0, NULL);
    vkCmdPushConstants(cbs[0], pl, VK_SHADER_STAGE_COMPUTE_BIT, 0, sizeof(count), &count);
    vkCmdDispatch(cbs[0], (COUNT + 63) / 64, 1, 1);
    buffer_barrier(cbs[0], out_dev.buf, VK_ACCESS_SHADER_WRITE_BIT, VK_ACCESS_TRANSFER_READ_BIT,
                   VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT);
    VK_CHECK(vkEndCommandBuffer(cbs[0]));

    /* cb1: download output to the host-visible readback buffer */
    VK_CHECK(vkBeginCommandBuffer(cbs[1], &cbbi));
    vkCmdCopyBuffer(cbs[1], out_dev.buf, readback.buf, 1, &region);
    buffer_barrier(cbs[1], readback.buf, VK_ACCESS_TRANSFER_WRITE_BIT, VK_ACCESS_HOST_READ_BIT,
                   VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_HOST_BIT);
    VK_CHECK(vkEndCommandBuffer(cbs[1]));

    /* cbloop (reusable): in = out, out = in * 2 + 1 */
    cbbi.flags = VK_COMMAND_BUFFER_USAGE_SIMULTANEOUS_USE_BIT;
    VK_CHECK(vkBeginCommandBuffer(cbloop, &cbbi));
    buffer_barrier(cbloop, out_dev.buf, VK_ACCESS_SHADER_WRITE_BIT, VK_ACCESS_TRANSFER_READ_BIT,
                   VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT);
    vkCmdCopyBuffer(cbloop, out_dev.buf, in_dev.buf, 1, &region);
    buffer_barrier(cbloop, in_dev.buf, VK_ACCESS_TRANSFER_WRITE_BIT, VK_ACCESS_SHADER_READ_BIT,
                   VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT);
    vkCmdBindPipeline(cbloop, VK_PIPELINE_BIND_POINT_COMPUTE, pipe);
    vkCmdBindDescriptorSets(cbloop, VK_PIPELINE_BIND_POINT_COMPUTE, pl, 0, 1, &ds, 0, NULL);
    vkCmdPushConstants(cbloop, pl, VK_SHADER_STAGE_COMPUTE_BIT, 0, sizeof(count), &count);
    vkCmdDispatch(cbloop, (COUNT + 63) / 64, 1, 1);
    VK_CHECK(vkEndCommandBuffer(cbloop));
    /* cbdl: out -> readback */
    VK_CHECK(vkBeginCommandBuffer(cbdl, &cbbi));
    buffer_barrier(cbdl, out_dev.buf, VK_ACCESS_SHADER_WRITE_BIT, VK_ACCESS_TRANSFER_READ_BIT,
                   VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT);
    vkCmdCopyBuffer(cbdl, out_dev.buf, readback.buf, 1, &region);
    buffer_barrier(cbdl, readback.buf, VK_ACCESS_TRANSFER_WRITE_BIT, VK_ACCESS_HOST_READ_BIT,
                   VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_HOST_BIT);
    VK_CHECK(vkEndCommandBuffer(cbdl));

    logmsg("creating timeline semaphore (initial value 0)");
    stci.semaphoreType = VK_SEMAPHORE_TYPE_TIMELINE;
    stci.initialValue = 0;
    sci.pNext = &stci;
    VK_CHECK(vkCreateSemaphore(device, &sci, NULL, &timeline));

    memset(tsi, 0, sizeof(tsi));
    memset(si, 0, sizeof(si));
    /* submit 0: signal 1 */
    tsi[0].sType = VK_STRUCTURE_TYPE_TIMELINE_SEMAPHORE_SUBMIT_INFO;
    tsi[0].signalSemaphoreValueCount = 1;
    tsi[0].pSignalSemaphoreValues = &sig1;
    si[0].sType = VK_STRUCTURE_TYPE_SUBMIT_INFO;
    si[0].pNext = &tsi[0];
    si[0].commandBufferCount = 1;
    si[0].pCommandBuffers = &cbs[0];
    si[0].signalSemaphoreCount = 1;
    si[0].pSignalSemaphores = &timeline;
    /* submit 1: wait 1 (GPU side), signal 2 */
    tsi[1].sType = VK_STRUCTURE_TYPE_TIMELINE_SEMAPHORE_SUBMIT_INFO;
    tsi[1].waitSemaphoreValueCount = 1;
    tsi[1].pWaitSemaphoreValues = &wait1;
    tsi[1].signalSemaphoreValueCount = 1;
    tsi[1].pSignalSemaphoreValues = &sig2;
    si[1].sType = VK_STRUCTURE_TYPE_SUBMIT_INFO;
    si[1].pNext = &tsi[1];
    si[1].waitSemaphoreCount = 1;
    si[1].pWaitSemaphores = &timeline;
    si[1].pWaitDstStageMask = &wait_stage;
    si[1].commandBufferCount = 1;
    si[1].pCommandBuffers = &cbs[1];
    si[1].signalSemaphoreCount = 1;
    si[1].pSignalSemaphores = &timeline;

    logmsg("vkQueueSubmit (2 batches: upload+dispatch -> timeline 1 -> download -> timeline 2)");
    t0 = now_ms();
    VK_CHECK(vkQueueSubmit(queue, 2, si, VK_NULL_HANDLE));

    logmsg("vkWaitSemaphores(timeline >= 2, timeout %u ms)", WAIT_TIMEOUT_MS);
    swi.semaphoreCount = 1;
    swi.pSemaphores = &timeline;
    swi.pValues = &wait_value;
    r = vkWaitSemaphores(device, &swi, (uint64_t)WAIT_TIMEOUT_MS * 1000000ull);
    vkGetSemaphoreCounterValue(device, timeline, &counter);
    if (r != VK_SUCCESS)
        return fail("vkWaitSemaphores returned %s, timeline value %llu", vk_res(r),
                    (unsigned long long)counter);
    logmsg("GPU work done in %.2f ms, timeline value %llu", now_ms() - t0, (unsigned long long)counter);

    for (i = 0; i < COUNT; i++) {
        uint32_t want = src[i] * 2u + 1u;
        if (dst[i] != want) {
            if (bad < 5)
                logmsg("mismatch at %u: got 0x%08x want 0x%08x", i, dst[i], want);
            bad++;
        }
    }
    if (bad)
        return fail("%u of %u elements wrong", bad, COUNT);
    logmsg("all %u elements correct", COUNT);

    g_queue = queue;
    g_timeline = timeline;
    g_tl = 2;
    rpoolci.queueFamilyIndex = qfam;
    rpoolci.flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT;
    VK_CHECK(vkCreateCommandPool(device, &rpoolci, NULL, &g_pool));

    if (iters > 0) {
        logmsg("compute loop: %d submits of copy+dispatch (timeline value per submit)", iters);
        t0 = now_ms();
        for (it = 0; it < iters; it++) {
            VkTimelineSemaphoreSubmitInfo lt = {.sType = VK_STRUCTURE_TYPE_TIMELINE_SEMAPHORE_SUBMIT_INFO};
            VkSubmitInfo ls = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO};
            uint64_t v = ++g_tl;
            lt.signalSemaphoreValueCount = 1;
            lt.pSignalSemaphoreValues = &v;
            ls.pNext = &lt;
            ls.commandBufferCount = 1;
            ls.pCommandBuffers = &cbloop;
            ls.signalSemaphoreCount = 1;
            ls.pSignalSemaphores = &timeline;
            VK_CHECK(vkQueueSubmit(queue, 1, &ls, VK_NULL_HANDLE));
            if (it % 50 == 49) {
                swi.pValues = &v;
                r = vkWaitSemaphores(device, &swi, (uint64_t)WAIT_TIMEOUT_MS * 1000000ull);
                if (r != VK_SUCCESS)
                    return fail("compute loop: wait for %llu returned %s", (unsigned long long)v,
                                vk_res(r));
            }
        }
        for (i = 0; i < COUNT; i++)
            dst[i] = 0xDEADBEEFu;
        if (submit_wait(cbdl, "compute loop download"))
            return 1;
        logmsg("compute loop: %d submits in %.1f ms", iters, now_ms() - t0);
        for (i = 0; i < COUNT; i++) {
            uint32_t want = src[i];
            for (it = 0; it <= iters; it++)
                want = want * 2u + 1u;
            if (dst[i] != want) {
                if (bad < 5)
                    logmsg("compute loop mismatch at %u: got 0x%08x want 0x%08x", i, dst[i], want);
                bad++;
            }
        }
        if (bad)
            return fail("compute loop: %u of %u elements wrong", bad, COUNT);
        logmsg("compute loop: all %u elements correct", COUNT);
    }
    if (!quick && alloc_sweep())
        return 1;
    if (wsi && present_loop(inst, pdev, qfam))
        return 1;

    logmsg("cleanup");
    VK_CHECK(vkDeviceWaitIdle(device));
    vkDestroySemaphore(device, timeline, NULL);
    vkDestroyCommandPool(device, g_pool, NULL);
    vkDestroyCommandPool(device, cpool, NULL);
    vkDestroyDescriptorPool(device, dpool, NULL);
    vkDestroyPipeline(device, pipe, NULL);
    vkDestroyPipelineLayout(device, pl, NULL);
    vkDestroyDescriptorSetLayout(device, dsl, NULL);
    vkDestroyShaderModule(device, sm, NULL);
    destroy_buffer(&staging);
    destroy_buffer(&in_dev);
    destroy_buffer(&out_dev);
    destroy_buffer(&readback);
    vkDestroyDevice(device, NULL);
    vkDestroyInstance(inst, NULL);
    return pass();
}
