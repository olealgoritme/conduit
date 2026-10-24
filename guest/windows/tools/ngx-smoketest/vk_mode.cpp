// ngx-smoketest, Vulkan mode: instance and device with the extensions NGX
// asks for, NGX init, DLSS SR feature, N evaluations with timestamps,
// readback and checks.
#include "smoketest.h"

#define VK_NO_PROTOTYPES
#include <vulkan/vulkan.h>

#include "nvsdk_ngx_vk.h"
#include "nvsdk_ngx_helpers_vk.h"


#define VK_GLOBAL(X) X(vkCreateInstance) X(vkEnumerateInstanceExtensionProperties) X(vkEnumerateInstanceVersion)
#define VK_INSTANCE(X)                                                                         \
    X(vkDestroyInstance) X(vkEnumeratePhysicalDevices) X(vkGetPhysicalDeviceProperties)        \
    X(vkGetPhysicalDeviceProperties2) X(vkGetPhysicalDeviceFeatures2)                           \
    X(vkGetPhysicalDeviceQueueFamilyProperties) X(vkGetPhysicalDeviceMemoryProperties)          \
    X(vkGetPhysicalDeviceFormatProperties) X(vkEnumerateDeviceExtensionProperties)              \
    X(vkCreateDevice) X(vkGetDeviceProcAddr)
#define VK_DEVICE(X)                                                                           \
    X(vkDestroyDevice) X(vkGetDeviceQueue) X(vkCreateCommandPool) X(vkDestroyCommandPool)      \
    X(vkAllocateCommandBuffers) X(vkBeginCommandBuffer) X(vkEndCommandBuffer)                  \
    X(vkResetCommandBuffer) X(vkQueueSubmit) X(vkCreateFence) X(vkDestroyFence)                \
    X(vkWaitForFences) X(vkResetFences) X(vkCreateImage) X(vkDestroyImage)                     \
    X(vkGetImageMemoryRequirements) X(vkAllocateMemory) X(vkFreeMemory) X(vkBindImageMemory)   \
    X(vkCreateImageView) X(vkDestroyImageView) X(vkCreateBuffer) X(vkDestroyBuffer)            \
    X(vkGetBufferMemoryRequirements) X(vkBindBufferMemory) X(vkMapMemory) X(vkUnmapMemory)     \
    X(vkInvalidateMappedMemoryRanges) X(vkCmdPipelineBarrier) X(vkCmdCopyBufferToImage)        \
    X(vkCmdCopyImageToBuffer) X(vkCmdClearColorImage) X(vkCreateQueryPool)                     \
    X(vkDestroyQueryPool) X(vkCmdResetQueryPool) X(vkCmdWriteTimestamp) X(vkGetQueryPoolResults)

#define DECL(n) static PFN_##n n;
VK_GLOBAL(DECL)
VK_INSTANCE(DECL)
VK_DEVICE(DECL)
static PFN_vkGetInstanceProcAddr gipa;

struct Img {
    VkImage img;
    VkDeviceMemory mem;
    VkImageView view;
    VkFormat fmt;
    unsigned w, h;
};
struct Buf {
    VkBuffer buf;
    VkDeviceMemory mem;
    void *map;
    VkDeviceSize size;
    bool coherent;
};

static VkInstance inst;
static VkPhysicalDevice pd;
static VkDevice dev;
static VkQueue queue;
static uint32_t qfam;
static VkCommandPool pool;
static VkCommandBuffer cmd;
static VkFence fence;
static VkQueryPool qpool;
static VkPhysicalDeviceMemoryProperties memprops;
static Img color, depth, mvec, output;
static Buf upload, readback;
static bool ngx_inited;
static NVSDK_NGX_Parameter *caps;
static NVSDK_NGX_Handle *feature;

static const char *vk_name(VkResult r)
{
    switch (r) {
#define C(x) case x: return #x;
    C(VK_SUCCESS) C(VK_NOT_READY) C(VK_TIMEOUT) C(VK_INCOMPLETE)
    C(VK_ERROR_OUT_OF_HOST_MEMORY) C(VK_ERROR_OUT_OF_DEVICE_MEMORY) C(VK_ERROR_INITIALIZATION_FAILED)
    C(VK_ERROR_DEVICE_LOST) C(VK_ERROR_MEMORY_MAP_FAILED) C(VK_ERROR_LAYER_NOT_PRESENT)
    C(VK_ERROR_EXTENSION_NOT_PRESENT) C(VK_ERROR_FEATURE_NOT_PRESENT) C(VK_ERROR_INCOMPATIBLE_DRIVER)
    C(VK_ERROR_TOO_MANY_OBJECTS) C(VK_ERROR_FORMAT_NOT_SUPPORTED) C(VK_ERROR_UNKNOWN)
#undef C
    default: return "VkResult(other)";
    }
}

static bool vkcheck(const char *what, VkResult r)
{
    if (r == VK_SUCCESS)
        return true;
    fail("%s -> %d %s", what, (int)r, vk_name(r));
    if (r == VK_ERROR_DEVICE_LOST)
        hang("device lost; stopping");
    return false;
}

static bool load_device_funcs()
{
    bool all = true;
#define LOAD(n)                                                       \
    n = (PFN_##n)vkGetDeviceProcAddr(dev, #n);                        \
    if (!n) { fail("vkGetDeviceProcAddr(%s) = NULL", #n); all = false; }
    VK_DEVICE(LOAD)
#undef LOAD
    return all;
}

static void begin_cmd()
{
    vkResetCommandBuffer(cmd, 0);
    VkCommandBufferBeginInfo bi = {VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};
    bi.flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT;
    vkBeginCommandBuffer(cmd, &bi);
}

// Ends the command buffer, submits it and waits for it (with the timeout).
static bool submit_wait(const char *what, double *cpu_ms)
{
    if (!vkcheck("vkEndCommandBuffer", vkEndCommandBuffer(cmd)))
        return false;
    VkSubmitInfo si = {VK_STRUCTURE_TYPE_SUBMIT_INFO};
    si.commandBufferCount = 1;
    si.pCommandBuffers = &cmd;
    double t0 = now_ms();
    if (!vkcheck("vkQueueSubmit", vkQueueSubmit(queue, 1, &si, fence)))
        return false;
    VkResult r = vkWaitForFences(dev, 1, &fence, VK_TRUE, (uint64_t)g_opt.timeout_ms * 1000000ull);
    if (cpu_ms)
        *cpu_ms = now_ms() - t0;
    if (r == VK_TIMEOUT)
        hang("%s: fence not signaled after %lu ms (GPU hang?)", what, (unsigned long)g_opt.timeout_ms);
    if (!vkcheck("vkWaitForFences", r))
        return false;
    vkResetFences(dev, 1, &fence);
    return true;
}

static bool find_mem(uint32_t bits, VkMemoryPropertyFlags want, uint32_t *idx)
{
    for (uint32_t i = 0; i < memprops.memoryTypeCount; i++)
        if ((bits & (1u << i)) && (memprops.memoryTypes[i].propertyFlags & want) == want) {
            *idx = i;
            return true;
        }
    return false;
}

static bool storage_ok(VkFormat f)
{
    VkFormatProperties fp;
    vkGetPhysicalDeviceFormatProperties(pd, f, &fp);
    return (fp.optimalTilingFeatures & VK_FORMAT_FEATURE_STORAGE_IMAGE_BIT) != 0;
}

static bool create_image(Img *im, const char *name, VkFormat fmt, unsigned w, unsigned h, VkImageUsageFlags usage)
{
    im->fmt = fmt;
    im->w = w;
    im->h = h;
    VkImageCreateInfo ci = {VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO};
    ci.imageType = VK_IMAGE_TYPE_2D;
    ci.format = fmt;
    ci.extent = {w, h, 1};
    ci.mipLevels = 1;
    ci.arrayLayers = 1;
    ci.samples = VK_SAMPLE_COUNT_1_BIT;
    ci.tiling = VK_IMAGE_TILING_OPTIMAL;
    ci.usage = usage;
    ci.initialLayout = VK_IMAGE_LAYOUT_UNDEFINED;
    char what[96];
    snprintf(what, sizeof(what), "vkCreateImage(%s %ux%u format %d)", name, w, h, (int)fmt);
    if (!vkcheck(what, vkCreateImage(dev, &ci, NULL, &im->img)))
        return false;
    VkMemoryRequirements mr;
    vkGetImageMemoryRequirements(dev, im->img, &mr);
    VkMemoryAllocateInfo ai = {VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO};
    ai.allocationSize = mr.size;
    if (!find_mem(mr.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT, &ai.memoryTypeIndex) &&
        !find_mem(mr.memoryTypeBits, 0, &ai.memoryTypeIndex)) {
        fail("%s: no memory type", name);
        return false;
    }
    if (!vkcheck("vkAllocateMemory(image)", vkAllocateMemory(dev, &ai, NULL, &im->mem)) ||
        !vkcheck("vkBindImageMemory", vkBindImageMemory(dev, im->img, im->mem, 0)))
        return false;
    VkImageViewCreateInfo vi = {VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO};
    vi.image = im->img;
    vi.viewType = VK_IMAGE_VIEW_TYPE_2D;
    vi.format = fmt;
    vi.subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1};
    return vkcheck("vkCreateImageView", vkCreateImageView(dev, &vi, NULL, &im->view));
}

static bool create_buffer(Buf *b, VkDeviceSize size, VkBufferUsageFlags usage, bool for_reading)
{
    b->size = size;
    VkBufferCreateInfo ci = {VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO};
    ci.size = size;
    ci.usage = usage;
    if (!vkcheck("vkCreateBuffer", vkCreateBuffer(dev, &ci, NULL, &b->buf)))
        return false;
    VkMemoryRequirements mr;
    vkGetBufferMemoryRequirements(dev, b->buf, &mr);
    VkMemoryAllocateInfo ai = {VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO};
    ai.allocationSize = mr.size;
    const VkMemoryPropertyFlags vis = VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT;
    const VkMemoryPropertyFlags coh = VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;
    const VkMemoryPropertyFlags cached = VK_MEMORY_PROPERTY_HOST_CACHED_BIT;
    bool found = (for_reading && find_mem(mr.memoryTypeBits, vis | coh | cached, &ai.memoryTypeIndex)) ||
                 find_mem(mr.memoryTypeBits, vis | coh, &ai.memoryTypeIndex) ||
                 find_mem(mr.memoryTypeBits, vis, &ai.memoryTypeIndex);
    if (!found) {
        fail("no host-visible memory type for a buffer");
        return false;
    }
    b->coherent = (memprops.memoryTypes[ai.memoryTypeIndex].propertyFlags & coh) != 0;
    if (!vkcheck("vkAllocateMemory(buffer)", vkAllocateMemory(dev, &ai, NULL, &b->mem)) ||
        !vkcheck("vkBindBufferMemory", vkBindBufferMemory(dev, b->buf, b->mem, 0)) ||
        !vkcheck("vkMapMemory", vkMapMemory(dev, b->mem, 0, VK_WHOLE_SIZE, 0, &b->map)))
        return false;
    return true;
}

static void image_barrier(VkImage img, VkImageLayout from, VkImageLayout to, VkPipelineStageFlags src,
                          VkAccessFlags src_access, VkPipelineStageFlags dst, VkAccessFlags dst_access)
{
    VkImageMemoryBarrier b = {VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER};
    b.srcAccessMask = src_access;
    b.dstAccessMask = dst_access;
    b.oldLayout = from;
    b.newLayout = to;
    b.srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED;
    b.dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED;
    b.image = img;
    b.subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1};
    vkCmdPipelineBarrier(cmd, src, dst, 0, 0, NULL, 0, NULL, 1, &b);
}

static void memory_barrier(VkPipelineStageFlags src, VkAccessFlags sa, VkPipelineStageFlags dst, VkAccessFlags da)
{
    VkMemoryBarrier b = {VK_STRUCTURE_TYPE_MEMORY_BARRIER};
    b.srcAccessMask = sa;
    b.dstAccessMask = da;
    vkCmdPipelineBarrier(cmd, src, dst, 0, 1, &b, 0, NULL, 0, NULL);
}

static NVSDK_NGX_Resource_VK ngx_res(const Img &im, bool rw)
{
    VkImageSubresourceRange range = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1};
    return NVSDK_NGX_Create_ImageView_Resource_VK(im.view, im.img, range, im.fmt, im.w, im.h, rw);
}

/* ---- extension lists ---- */

struct NameList {
    char names[64][VK_MAX_EXTENSION_NAME_SIZE];
    int n;
    void add(const char *s)
    {
        for (int i = 0; i < n; i++)
            if (!strcmp(names[i], s))
                return;
        if (n < 64)
            snprintf(names[n++], VK_MAX_EXTENSION_NAME_SIZE, "%s", s);
    }
    bool has(const char *s) const
    {
        for (int i = 0; i < n; i++)
            if (!strcmp(names[i], s))
                return true;
        return false;
    }
};

static bool ext_available(const VkExtensionProperties *p, uint32_t n, const char *name)
{
    for (uint32_t i = 0; i < n; i++)
        if (!strcmp(p[i].extensionName, name))
            return true;
    return false;
}

// Enables the wanted extensions the driver has; prints and fails on the rest.
static int pick_extensions(const char *kind, const NameList &want, const VkExtensionProperties *avail,
                           uint32_t navail, const char **enabled, int nenabled)
{
    char missing[1024] = "";
    for (int i = 0; i < want.n; i++) {
        if (ext_available(avail, navail, want.names[i]))
            enabled[nenabled++] = want.names[i];
        else {
            strncat(missing, " ", sizeof(missing) - strlen(missing) - 1);
            strncat(missing, want.names[i], sizeof(missing) - strlen(missing) - 1);
        }
    }
    for (int i = 0; i < nenabled; i++)
        out("       enable %s extension %s", kind, enabled[i]);
    if (missing[0]) {
        fail("%s extensions NGX wants but the driver does not have:%s", kind, missing);
        if (strstr(missing, "VK_NVX_"))
            out("       (NVK offers VK_NVX_binary_import / VK_NVX_image_view_handle only with NVK_DLSS=1 or "
                "HKLM\\SOFTWARE\\Helios Dlss=1)");
    } else
        ok("all %d %s extensions NGX wants are there", want.n, kind);
    return nenabled;
}

/* direct driver loading, as guest/nvk-rm/tests/vk_direct_driver.h */
static VkDirectDriverLoadingInfoLUNARG dd_info;
static VkDirectDriverLoadingListLUNARG dd_list;

static bool direct_driver(VkInstanceCreateInfo *ici, const char **exts, int *n)
{
    const char *path = getenv("VK_DIRECT_DRIVER");
    if (!path || !*path)
        return true;
    HMODULE dll = LoadLibraryA(path);
    if (!dll) {
        fail("VK_DIRECT_DRIVER: LoadLibrary(%s) failed: %lu", path, (unsigned long)GetLastError());
        return false;
    }
    auto icd_gipa = (PFN_vkGetInstanceProcAddrLUNARG)(void *)GetProcAddress(dll, "vk_icdGetInstanceProcAddr");
    if (!icd_gipa) {
        fail("VK_DIRECT_DRIVER: %s has no vk_icdGetInstanceProcAddr", path);
        return false;
    }
    dd_info = {VK_STRUCTURE_TYPE_DIRECT_DRIVER_LOADING_INFO_LUNARG};
    dd_info.pfnGetInstanceProcAddr = icd_gipa;
    dd_list = {VK_STRUCTURE_TYPE_DIRECT_DRIVER_LOADING_LIST_LUNARG};
    dd_list.pNext = ici->pNext;
    dd_list.mode = VK_DIRECT_DRIVER_LOADING_MODE_EXCLUSIVE_LUNARG;
    dd_list.driverCount = 1;
    dd_list.pDrivers = &dd_info;
    ici->pNext = &dd_list;
    exts[(*n)++] = VK_LUNARG_DIRECT_DRIVER_LOADING_EXTENSION_NAME;
    out("       VK_DIRECT_DRIVER: %s (exclusive)", path);
    return true;
}

static void cleanup()
{
    step("shutdown");
    if (feature)
        ngx_check("NVSDK_NGX_VULKAN_ReleaseFeature", NVSDK_NGX_VULKAN_ReleaseFeature(feature));
    if (caps)
        ngx_check("NVSDK_NGX_VULKAN_DestroyParameters", NVSDK_NGX_VULKAN_DestroyParameters(caps));
    if (ngx_inited)
        ngx_check("NVSDK_NGX_VULKAN_Shutdown1", NVSDK_NGX_VULKAN_Shutdown1(dev));
    feature = NULL;
    caps = NULL;
    ngx_inited = false;
    if (dev) {
        Img *imgs[] = {&color, &depth, &mvec, &output};
        for (Img *im : imgs) {
            if (im->view) vkDestroyImageView(dev, im->view, NULL);
            if (im->img) vkDestroyImage(dev, im->img, NULL);
            if (im->mem) vkFreeMemory(dev, im->mem, NULL);
        }
        Buf *bufs[] = {&upload, &readback};
        for (Buf *b : bufs) {
            if (b->buf) vkDestroyBuffer(dev, b->buf, NULL);
            if (b->mem) vkFreeMemory(dev, b->mem, NULL);
        }
        if (qpool) vkDestroyQueryPool(dev, qpool, NULL);
        if (fence) vkDestroyFence(dev, fence, NULL);
        if (pool) vkDestroyCommandPool(dev, pool, NULL);
        vkDestroyDevice(dev, NULL);
        dev = NULL;
    }
    if (inst) {
        vkDestroyInstance(inst, NULL);
        inst = NULL;
    }
    ok("Vulkan objects destroyed");
}

static int finish()
{
    cleanup();
    return failures() ? 1 : 0;
}

int run_vulkan()
{
    step("vulkan loader");
    HMODULE vk = LoadLibraryA("vulkan-1.dll");
    if (!vk) {
        fail("LoadLibrary(vulkan-1.dll) failed: %lu", (unsigned long)GetLastError());
        return 1;
    }
    gipa = (PFN_vkGetInstanceProcAddr)(void *)GetProcAddress(vk, "vkGetInstanceProcAddr");
    if (!gipa) {
        fail("vulkan-1.dll has no vkGetInstanceProcAddr");
        return 1;
    }
#define LOADG(n) n = (PFN_##n)gipa(NULL, #n);
    VK_GLOBAL(LOADG)
#undef LOADG
    uint32_t iver = VK_API_VERSION_1_0;
    if (vkEnumerateInstanceVersion)
        vkEnumerateInstanceVersion(&iver);
    ok("vulkan-1.dll loaded, instance version %u.%u.%u", VK_API_VERSION_MAJOR(iver), VK_API_VERSION_MINOR(iver),
       VK_API_VERSION_PATCH(iver));

    NVSDK_NGX_FeatureCommonInfo ci;
    NVSDK_NGX_FeatureDiscoveryInfo di;
    ngx_common_info(&ci);
    ngx_discovery_info(&di, &ci);
    NameList want_inst = {}, want_dev = {};

    step("NGX required extensions");
    unsigned ic = 0, dc = 0;
    const char **ie = NULL, **de = NULL;
    if (ngx_check("NVSDK_NGX_VULKAN_RequiredExtensions",
                  NVSDK_NGX_VULKAN_RequiredExtensions(&ic, &ie, &dc, &de))) {
        for (unsigned i = 0; i < ic; i++) {
            out("       instance: %s", ie[i]);
            want_inst.add(ie[i]);
        }
        for (unsigned i = 0; i < dc; i++) {
            out("       device:   %s", de[i]);
            want_dev.add(de[i]);
        }
    }

    step("NGX instance extension requirements");
    uint32_t nprop = 0;
    VkExtensionProperties *props = NULL;
    if (ngx_check("NVSDK_NGX_VULKAN_GetFeatureInstanceExtensionRequirements",
                  NVSDK_NGX_VULKAN_GetFeatureInstanceExtensionRequirements(&di, &nprop, &props))) {
        for (uint32_t i = 0; i < nprop && props; i++) {
            out("       instance: %s", props[i].extensionName);
            want_inst.add(props[i].extensionName);
        }
    }

    step("instance extensions");
    uint32_t navail = 0;
    vkEnumerateInstanceExtensionProperties(NULL, &navail, NULL);
    VkExtensionProperties *avail = (VkExtensionProperties *)calloc(navail + 1, sizeof(*avail));
    vkEnumerateInstanceExtensionProperties(NULL, &navail, avail);
    const char *iexts[72];
    int niexts = pick_extensions("instance", want_inst, avail, navail, iexts, 0);
    free(avail);

    step("vkCreateInstance");
    VkApplicationInfo app = {VK_STRUCTURE_TYPE_APPLICATION_INFO};
    app.pApplicationName = "ngx-smoketest";
    app.pEngineName = "ngx-smoketest";
    app.apiVersion = iver >= VK_API_VERSION_1_3 ? VK_API_VERSION_1_3 : iver;
    VkInstanceCreateInfo ici = {VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO};
    ici.pApplicationInfo = &app;
    if (!direct_driver(&ici, iexts, &niexts))
        return 1;
    ici.enabledExtensionCount = (uint32_t)niexts;
    ici.ppEnabledExtensionNames = iexts;
    if (!vkcheck("vkCreateInstance", vkCreateInstance(&ici, NULL, &inst)))
        return 1;
#define LOADI(n) n = (PFN_##n)gipa(inst, #n);
    VK_INSTANCE(LOADI)
#undef LOADI
    if (!vkGetPhysicalDeviceProperties2 || !vkGetPhysicalDeviceFeatures2) {
        fail("no vkGetPhysicalDeviceProperties2/Features2 (Vulkan 1.1 needed)");
        return finish();
    }
    ok("Vulkan %u.%u instance", VK_API_VERSION_MAJOR(app.apiVersion), VK_API_VERSION_MINOR(app.apiVersion));

    step("physical device");
    uint32_t npd = 0;
    vkEnumeratePhysicalDevices(inst, &npd, NULL);
    if (!npd) {
        fail("no Vulkan physical device");
        return finish();
    }
    VkPhysicalDevice pds[16];
    if (npd > 16)
        npd = 16;
    vkEnumeratePhysicalDevices(inst, &npd, pds);
    for (uint32_t i = 0; i < npd; i++) {
        VkPhysicalDeviceIDProperties idp = {VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ID_PROPERTIES};
        VkPhysicalDeviceDriverProperties drv = {VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRIVER_PROPERTIES};
        drv.pNext = &idp;
        VkPhysicalDeviceProperties2 p2 = {VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2};
        p2.pNext = &drv;
        vkGetPhysicalDeviceProperties2(pds[i], &p2);
        const VkPhysicalDeviceProperties &p = p2.properties;
        uint32_t luid_lo, luid_hi;
        memcpy(&luid_lo, idp.deviceLUID, 4);
        memcpy(&luid_hi, idp.deviceLUID + 4, 4);
        out("       [%u] %s  vendor 0x%04x device 0x%04x  Vulkan %u.%u.%u  driver 0x%08x  %s %s  LUID %s%08x:%08x",
            i, p.deviceName, p.vendorID, p.deviceID, VK_API_VERSION_MAJOR(p.apiVersion),
            VK_API_VERSION_MINOR(p.apiVersion), VK_API_VERSION_PATCH(p.apiVersion), p.driverVersion,
            drv.driverName, drv.driverInfo, idp.deviceLUIDValid ? "" : "(invalid) ", luid_hi, luid_lo);
    }
    uint32_t pick = g_opt.device < 0 ? 0 : (uint32_t)g_opt.device;
    if (pick >= npd) {
        fail("--device %d: only %u device(s)", g_opt.device, npd);
        return finish();
    }
    pd = pds[pick];
    VkPhysicalDeviceProperties props0;
    vkGetPhysicalDeviceProperties(pd, &props0);
    vkGetPhysicalDeviceMemoryProperties(pd, &memprops);
    ok("using [%u] %s (vendor 0x%04x, device 0x%04x)", pick, props0.deviceName, props0.vendorID,
       props0.deviceID);

    step("NGX feature requirements");
    NVSDK_NGX_FeatureRequirement req;
    memset(&req, 0, sizeof(req));
    if (ngx_check("NVSDK_NGX_VULKAN_GetFeatureRequirements",
                  NVSDK_NGX_VULKAN_GetFeatureRequirements(inst, pd, &di, &req))) {
        ngx_print_requirement(&req);
        if (req.FeatureSupported != NVSDK_NGX_FeatureSupportResult_Supported)
            fail("NGX reports SuperSampling as not supported on this device");
    }

    step("NGX device extension requirements");
    nprop = 0;
    props = NULL;
    if (ngx_check("NVSDK_NGX_VULKAN_GetFeatureDeviceExtensionRequirements",
                  NVSDK_NGX_VULKAN_GetFeatureDeviceExtensionRequirements(inst, pd, &di, &nprop, &props))) {
        for (uint32_t i = 0; i < nprop && props; i++) {
            out("       device: %s", props[i].extensionName);
            want_dev.add(props[i].extensionName);
        }
    }

    step("device extensions");
    navail = 0;
    vkEnumerateDeviceExtensionProperties(pd, NULL, &navail, NULL);
    avail = (VkExtensionProperties *)calloc(navail + 1, sizeof(*avail));
    vkEnumerateDeviceExtensionProperties(pd, NULL, &navail, avail);
    // VK_EXT_buffer_device_address and VK_KHR_buffer_device_address must not
    // both be enabled; the KHR one (core in 1.2) wins.
    NameList want2 = {};
    for (int i = 0; i < want_dev.n; i++) {
        if (!strcmp(want_dev.names[i], "VK_EXT_buffer_device_address") &&
            (ext_available(avail, navail, "VK_KHR_buffer_device_address") ||
             props0.apiVersion >= VK_API_VERSION_1_2)) {
            out("       skip VK_EXT_buffer_device_address (VK_KHR_buffer_device_address / Vulkan 1.2 instead)");
            want2.add("VK_KHR_buffer_device_address");
            continue;
        }
        want2.add(want_dev.names[i]);
    }
    const char *dexts[72];
    int ndexts = pick_extensions("device", want2, avail, navail, dexts, 0);
    free(avail);

    step("vkCreateDevice");
    uint32_t nq = 0;
    vkGetPhysicalDeviceQueueFamilyProperties(pd, &nq, NULL);
    VkQueueFamilyProperties qf[16];
    if (nq > 16)
        nq = 16;
    vkGetPhysicalDeviceQueueFamilyProperties(pd, &nq, qf);
    qfam = UINT32_MAX;
    for (uint32_t i = 0; i < nq; i++)
        if ((qf[i].queueFlags & (VK_QUEUE_GRAPHICS_BIT | VK_QUEUE_COMPUTE_BIT)) ==
            (VK_QUEUE_GRAPHICS_BIT | VK_QUEUE_COMPUTE_BIT)) {
            qfam = i;
            break;
        }
    if (qfam == UINT32_MAX) {
        fail("no graphics+compute queue family");
        return finish();
    }
    uint32_t ts_bits = qf[qfam].timestampValidBits;
    // Every core feature the device has, except the robustness ones and
    // the capture/replay and multi-device address features.
    VkPhysicalDeviceVulkan13Features f13 = {VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES};
    VkPhysicalDeviceVulkan12Features f12 = {VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES};
    VkPhysicalDeviceVulkan11Features f11 = {VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_1_FEATURES};
    VkPhysicalDeviceFeatures2 f2 = {VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2};
    bool has13 = props0.apiVersion >= VK_API_VERSION_1_3 && app.apiVersion >= VK_API_VERSION_1_3;
    bool has12 = props0.apiVersion >= VK_API_VERSION_1_2 && app.apiVersion >= VK_API_VERSION_1_2;
    f2.pNext = &f11;
    f11.pNext = has12 ? (void *)&f12 : NULL;
    f12.pNext = has13 ? (void *)&f13 : NULL;
    vkGetPhysicalDeviceFeatures2(pd, &f2);
    f2.features.robustBufferAccess = VK_FALSE;
    f11.protectedMemory = VK_FALSE;
    f12.bufferDeviceAddressCaptureReplay = VK_FALSE;
    f12.bufferDeviceAddressMultiDevice = VK_FALSE;
    f13.robustImageAccess = VK_FALSE;
    out("       queue family %u, timestampValidBits %u, timestampPeriod %.3f ns; bufferDeviceAddress %u",
        qfam, ts_bits, props0.limits.timestampPeriod, has12 ? f12.bufferDeviceAddress : 0);
    float prio = 1.0f;
    VkDeviceQueueCreateInfo qci = {VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO};
    qci.queueFamilyIndex = qfam;
    qci.queueCount = 1;
    qci.pQueuePriorities = &prio;
    VkDeviceCreateInfo dci = {VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO};
    dci.pNext = &f2;
    dci.queueCreateInfoCount = 1;
    dci.pQueueCreateInfos = &qci;
    dci.enabledExtensionCount = (uint32_t)ndexts;
    dci.ppEnabledExtensionNames = dexts;
    if (!vkcheck("vkCreateDevice", vkCreateDevice(pd, &dci, NULL, &dev)))
        return finish();
    if (!load_device_funcs())
        return finish();
    ok("device created with %d extensions", ndexts);
    const char *nvx[] = {"vkCreateCuModuleNVX", "vkCreateCuFunctionNVX", "vkCmdCuLaunchKernelNVX",
                         "vkGetImageViewHandleNVX", "vkGetImageViewAddressNVX"};
    for (const char *n : nvx) {
        bool have = vkGetDeviceProcAddr(dev, n) != NULL;
        out("       %-26s %s", n, have ? "present" : "MISSING");
        if (!have)
            fail("%s missing", n);
    }
    vkGetDeviceQueue(dev, qfam, 0, &queue);
    VkCommandPoolCreateInfo pci = {VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO};
    pci.flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT;
    pci.queueFamilyIndex = qfam;
    VkCommandBufferAllocateInfo cai = {VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO};
    cai.commandBufferCount = 1;
    cai.level = VK_COMMAND_BUFFER_LEVEL_PRIMARY;
    VkFenceCreateInfo fci = {VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
    if (!vkcheck("vkCreateCommandPool", vkCreateCommandPool(dev, &pci, NULL, &pool)))
        return finish();
    cai.commandPool = pool;
    if (!vkcheck("vkAllocateCommandBuffers", vkAllocateCommandBuffers(dev, &cai, &cmd)) ||
        !vkcheck("vkCreateFence", vkCreateFence(dev, &fci, NULL, &fence)))
        return finish();

    step("NGX init");
    double t0 = now_ms();
    NVSDK_NGX_Result r;
    if (g_opt.use_app_id)
        r = NVSDK_NGX_VULKAN_Init(g_opt.app_id, g_opt.logdir, inst, pd, dev, gipa, vkGetDeviceProcAddr, &ci,
                                  NVSDK_NGX_Version_API);
    else
        r = NVSDK_NGX_VULKAN_Init_with_ProjectID(g_opt.project_id, NVSDK_NGX_ENGINE_TYPE_CUSTOM, "1.0",
                                                 g_opt.logdir, inst, pd, dev, gipa, vkGetDeviceProcAddr, &ci,
                                                 NVSDK_NGX_Version_API);
    out("       took %.1f ms", now_ms() - t0);
    ngx_inited = ngx_check(g_opt.use_app_id ? "NVSDK_NGX_VULKAN_Init" : "NVSDK_NGX_VULKAN_Init_with_ProjectID", r);
    report_loaded_modules();
    if (!ngx_inited)
        return finish();

    step("NGX capability parameters");
    if (!ngx_check("NVSDK_NGX_VULKAN_GetCapabilityParameters", NVSDK_NGX_VULKAN_GetCapabilityParameters(&caps)))
        return finish();
    if (!ngx_print_caps(caps)) {
        fail("SuperSampling.Available is not 1: DLSS is not available");
        return finish();
    }
    ok("SuperSampling available");

    step("render size");
    unsigned rw, rh, ow = g_opt.out_w, oh = g_opt.out_h;
    ngx_render_size(caps, &rw, &rh);

    step("scratch buffer size");
    size_t scratch = 0;
    r = NVSDK_NGX_VULKAN_GetScratchBufferSize(NVSDK_NGX_Feature_SuperSampling, caps, &scratch);
    if (NVSDK_NGX_SUCCEED(r))
        ok("NVSDK_NGX_VULKAN_GetScratchBufferSize -> 0x%08x %s: %zu bytes", (unsigned)r, ngx_result_name(r), scratch);
    else
        warn("NVSDK_NGX_VULKAN_GetScratchBufferSize -> 0x%08x %s", (unsigned)r, ngx_result_name(r));

    step("resources");
    VkImageUsageFlags in_usage = VK_IMAGE_USAGE_SAMPLED_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT |
                                 VK_IMAGE_USAGE_TRANSFER_SRC_BIT;
    if (!storage_ok(VK_FORMAT_R8G8B8A8_UNORM)) {
        fail("R8G8B8A8_UNORM has no storage-image support (DLSS writes the output as a storage image)");
        return finish();
    }
    if (!create_image(&color, "color", VK_FORMAT_R8G8B8A8_UNORM, rw, rh, in_usage | VK_IMAGE_USAGE_STORAGE_BIT) ||
        !create_image(&depth, "depth", VK_FORMAT_R32_SFLOAT, rw, rh,
                      in_usage | (storage_ok(VK_FORMAT_R32_SFLOAT) ? VK_IMAGE_USAGE_STORAGE_BIT : 0)) ||
        !create_image(&mvec, "motion vectors", VK_FORMAT_R16G16_SFLOAT, rw, rh,
                      in_usage | (storage_ok(VK_FORMAT_R16G16_SFLOAT) ? VK_IMAGE_USAGE_STORAGE_BIT : 0)) ||
        !create_image(&output, "output", VK_FORMAT_R8G8B8A8_UNORM, ow, oh,
                      VK_IMAGE_USAGE_STORAGE_BIT | VK_IMAGE_USAGE_SAMPLED_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT |
                          VK_IMAGE_USAGE_TRANSFER_DST_BIT))
        return finish();
    VkDeviceSize color_bytes = (VkDeviceSize)rw * rh * 4, depth_bytes = color_bytes, mv_bytes = color_bytes;
    if (!create_buffer(&upload, color_bytes + depth_bytes + mv_bytes, VK_BUFFER_USAGE_TRANSFER_SRC_BIT, false) ||
        !create_buffer(&readback, (VkDeviceSize)ow * oh * 4, VK_BUFFER_USAGE_TRANSFER_DST_BIT, true))
        return finish();
    uint8_t *host_color = (uint8_t *)malloc(color_bytes);
    make_color(host_color, rw, rh, (size_t)rw * 4);
    memcpy(upload.map, host_color, color_bytes);
    make_depth((float *)((uint8_t *)upload.map + color_bytes), rw, rh, (size_t)rw * 4);
    memset((uint8_t *)upload.map + color_bytes + depth_bytes, 0, mv_bytes); // RG16F zero motion
    bool timestamps = ts_bits > 0;
    if (timestamps) {
        VkQueryPoolCreateInfo qpi = {VK_STRUCTURE_TYPE_QUERY_POOL_CREATE_INFO};
        qpi.queryType = VK_QUERY_TYPE_TIMESTAMP;
        qpi.queryCount = 2;
        if (!vkcheck("vkCreateQueryPool", vkCreateQueryPool(dev, &qpi, NULL, &qpool)))
            timestamps = false;
    } else
        warn("the queue has no timestamps; GPU times are not measured");

    begin_cmd();
    Img *all[] = {&color, &depth, &mvec, &output};
    for (Img *im : all)
        image_barrier(im->img, VK_IMAGE_LAYOUT_UNDEFINED, VK_IMAGE_LAYOUT_GENERAL, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,
                      0, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_ACCESS_TRANSFER_WRITE_BIT);
    VkBufferImageCopy region = {};
    region.imageSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1};
    region.imageExtent = {rw, rh, 1};
    region.bufferOffset = 0;
    vkCmdCopyBufferToImage(cmd, upload.buf, color.img, VK_IMAGE_LAYOUT_GENERAL, 1, &region);
    region.bufferOffset = color_bytes;
    vkCmdCopyBufferToImage(cmd, upload.buf, depth.img, VK_IMAGE_LAYOUT_GENERAL, 1, &region);
    region.bufferOffset = color_bytes + depth_bytes;
    vkCmdCopyBufferToImage(cmd, upload.buf, mvec.img, VK_IMAGE_LAYOUT_GENERAL, 1, &region);
    VkClearColorValue clear;
    for (int c = 0; c < 4; c++)
        clear.float32[c] = kClear[c] / 255.0f;
    VkImageSubresourceRange range = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1};
    vkCmdClearColorImage(cmd, output.img, VK_IMAGE_LAYOUT_GENERAL, &clear, 1, &range);
    memory_barrier(VK_PIPELINE_STAGE_TRANSFER_BIT, VK_ACCESS_TRANSFER_WRITE_BIT, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT,
                   VK_ACCESS_SHADER_READ_BIT | VK_ACCESS_SHADER_WRITE_BIT);
    if (!submit_wait("upload", NULL))
        return finish();
    ok("color %ux%u RGBA8, depth R32F, motion vectors RG16F (zero), output %ux%u RGBA8 (cleared to 255,0,255)",
       rw, rh, ow, oh);

    step("create DLSS feature");
    ngx_set_create_hints(caps);
    NVSDK_NGX_DLSS_Create_Params cp;
    memset(&cp, 0, sizeof(cp));
    cp.Feature.InWidth = rw;
    cp.Feature.InHeight = rh;
    cp.Feature.InTargetWidth = ow;
    cp.Feature.InTargetHeight = oh;
    cp.Feature.InPerfQualityValue = g_opt.quality;
    cp.InFeatureCreateFlags = ngx_create_flags();
    begin_cmd();
    t0 = now_ms();
    r = NGX_VULKAN_CREATE_DLSS_EXT1(dev, cmd, 1, 1, &feature, caps, &cp);
    double t_create = now_ms() - t0;
    if (!ngx_check("NGX_VULKAN_CREATE_DLSS_EXT1 (NVSDK_NGX_VULKAN_CreateFeature1)", r)) {
        vkEndCommandBuffer(cmd);
        feature = NULL;
        return finish();
    }
    double t_submit = 0;
    if (!submit_wait("feature creation", &t_submit))
        return finish();
    ok("feature created (CPU %.1f ms, its command buffer %.1f ms)", t_create, t_submit);

    step("evaluate");
    Timings tm;
    timings_alloc(&tm, g_opt.frames);
    NVSDK_NGX_Resource_VK r_color = ngx_res(color, false), r_depth = ngx_res(depth, false),
                          r_mv = ngx_res(mvec, false), r_out = ngx_res(output, true);
    int done = 0;
    for (int f = 0; f < g_opt.frames; f++) {
        begin_cmd();
        if (timestamps) {
            vkCmdResetQueryPool(cmd, qpool, 0, 2);
            vkCmdWriteTimestamp(cmd, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, qpool, 0);
        }
        NVSDK_NGX_VK_DLSS_Eval_Params ep;
        memset(&ep, 0, sizeof(ep));
        ep.Feature.pInColor = &r_color;
        ep.Feature.pInOutput = &r_out;
        ep.pInDepth = &r_depth;
        ep.pInMotionVectors = &r_mv;
        jitter(f, &ep.InJitterOffsetX, &ep.InJitterOffsetY);
        ep.InRenderSubrectDimensions.Width = rw;
        ep.InRenderSubrectDimensions.Height = rh;
        ep.InReset = f == 0;
        ep.InMVScaleX = 1.0f;
        ep.InMVScaleY = 1.0f;
        double c0 = now_ms();
        r = NGX_VULKAN_EVALUATE_DLSS_EXT(cmd, feature, caps, &ep);
        tm.cpu_eval[f] = now_ms() - c0;
        if (NVSDK_NGX_FAILED(r)) {
            fail("frame %d: NGX_VULKAN_EVALUATE_DLSS_EXT -> 0x%08x %s", f, (unsigned)r, ngx_result_name(r));
            vkEndCommandBuffer(cmd);
            break;
        }
        if (timestamps)
            vkCmdWriteTimestamp(cmd, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, qpool, 1);
        char what[48];
        snprintf(what, sizeof(what), "evaluate frame %d", f);
        if (!submit_wait(what, &tm.cpu_frame[f]))
            break;
        tm.gpu[f] = -1;
        if (timestamps) {
            uint64_t ts[2];
            if (vkGetQueryPoolResults(dev, qpool, 0, 2, sizeof(ts), ts, sizeof(uint64_t), VK_QUERY_RESULT_64_BIT) ==
                VK_SUCCESS) {
                uint64_t mask = ts_bits >= 64 ? ~0ull : ((1ull << ts_bits) - 1);
                tm.gpu[f] = (double)((ts[1] - ts[0]) & mask) * props0.limits.timestampPeriod / 1e6;
            }
        }
        if (f < 3 || f == g_opt.frames - 1)
            out("       frame %3d: GPU %.3f ms, EvaluateFeature call %.3f ms, submit to signal %.3f ms", f, tm.gpu[f],
                tm.cpu_eval[f], tm.cpu_frame[f]);
        done++;
    }
    if (done == g_opt.frames)
        ok("%d frames evaluated (NGX result 0x%08x %s on the last)", done, (unsigned)r, ngx_result_name(r));
    else
        fail("only %d of %d frames evaluated", done, g_opt.frames);
    tm.n = done;
    timings_print(&tm);

    step("readback");
    begin_cmd();
    image_barrier(output.img, VK_IMAGE_LAYOUT_GENERAL, VK_IMAGE_LAYOUT_GENERAL, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT,
                  VK_ACCESS_SHADER_WRITE_BIT | VK_ACCESS_MEMORY_WRITE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT,
                  VK_ACCESS_TRANSFER_READ_BIT);
    VkBufferImageCopy rb = {};
    rb.imageSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1};
    rb.imageExtent = {ow, oh, 1};
    vkCmdCopyImageToBuffer(cmd, output.img, VK_IMAGE_LAYOUT_GENERAL, readback.buf, 1, &rb);
    memory_barrier(VK_PIPELINE_STAGE_TRANSFER_BIT, VK_ACCESS_TRANSFER_WRITE_BIT, VK_PIPELINE_STAGE_HOST_BIT,
                   VK_ACCESS_HOST_READ_BIT);
    if (submit_wait("readback", NULL)) {
        if (!readback.coherent) {
            VkMappedMemoryRange mr = {VK_STRUCTURE_TYPE_MAPPED_MEMORY_RANGE};
            mr.memory = readback.mem;
            mr.size = VK_WHOLE_SIZE;
            vkInvalidateMappedMemoryRanges(dev, 1, &mr);
        }
        check_output((const uint8_t *)readback.map, ow, oh, (size_t)ow * 4, host_color, rw, rh, (size_t)rw * 4);
    }
    free(host_color);
    return finish();
}
