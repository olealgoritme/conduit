/*
 * Checks NVK's helios_icd_interface_v2 (patch 0023) the way the Helios D3D11
 * UMD uses it, without the UMD: loads vulkan_nouveau.dll directly (no
 * loader), reads deviceLUID, gets the interface table, makes a dedicated
 * 32 bpp image with the Helios export request, clears it and asks for a KMD
 * resource id (IMPORT_RM with layout). Optionally shows images on scanout 0
 * through the interface's scanout_present.
 *
 *   helios_icd_test.exe [dll] [linear|optimal] [scanout-seconds] [w h]
 *
 * Run with NVK_RM=1 and librmclient.dll next to the driver.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <windows.h>

#define VK_NO_PROTOTYPES
#include <vulkan/vulkan.h>

#include "../../windows/protocol/include/helios_icd_interface.h"

typedef VkResult (VKAPI_PTR *PFN_negotiate)(uint32_t *version);

static PFN_vkGetInstanceProcAddr gipa;
static VkInstance inst;
static VkDevice dev;
static PFN_vkGetDeviceProcAddr gdpa;

#define IFN(name) PFN_##name name = (PFN_##name)gipa(inst, #name)
#define DFN(name) PFN_##name name = (PFN_##name)gdpa(dev, #name)

static uint32_t find_type(VkPhysicalDevice pd, uint32_t bits, VkMemoryPropertyFlags want)
{
    IFN(vkGetPhysicalDeviceMemoryProperties);
    VkPhysicalDeviceMemoryProperties mp;
    vkGetPhysicalDeviceMemoryProperties(pd, &mp);
    for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
        if ((bits & (1u << i)) && (mp.memoryTypes[i].propertyFlags & want) == want)
            return i;
    return 0;
}

struct img {
    VkImage image;
    VkDeviceMemory mem;
};

static VkResult make_image(VkPhysicalDevice pd, uint32_t w, uint32_t h, VkImageTiling tiling,
                           struct img *out)
{
    DFN(vkCreateImage);
    DFN(vkGetImageMemoryRequirements);
    DFN(vkAllocateMemory);
    DFN(vkBindImageMemory);
    VkImageCreateInfo ici = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
        .imageType = VK_IMAGE_TYPE_2D,
        .format = VK_FORMAT_B8G8R8A8_UNORM,
        .extent = { w, h, 1 },
        .mipLevels = 1,
        .arrayLayers = 1,
        .samples = VK_SAMPLE_COUNT_1_BIT,
        .tiling = tiling,
        .usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT |
                 VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_SAMPLED_BIT,
        .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
    };
    VkResult r = vkCreateImage(dev, &ici, NULL, &out->image);
    if (r)
        return r;
    VkMemoryRequirements req;
    vkGetImageMemoryRequirements(dev, out->image, &req);
    struct helios_export_memory_resource_info hx = {
        .sType = HELIOS_STRUCTURE_TYPE_EXPORT_MEMORY_RESOURCE_INFO,
    };
    VkMemoryDedicatedAllocateInfo ded = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
        .pNext = &hx,
        .image = out->image,
    };
    VkMemoryAllocateInfo mai = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        .pNext = &ded,
        .allocationSize = req.size,
        .memoryTypeIndex = find_type(pd, req.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT),
    };
    r = vkAllocateMemory(dev, &mai, NULL, &out->mem);
    if (r)
        return r;
    return vkBindImageMemory(dev, out->image, out->mem, 0);
}

static VkResult clear_image(uint32_t qf, VkImage image, float r_, float g_, float b_)
{
    DFN(vkGetDeviceQueue);
    DFN(vkCreateCommandPool);
    DFN(vkAllocateCommandBuffers);
    DFN(vkBeginCommandBuffer);
    DFN(vkCmdPipelineBarrier);
    DFN(vkCmdClearColorImage);
    DFN(vkEndCommandBuffer);
    DFN(vkQueueSubmit);
    DFN(vkQueueWaitIdle);
    DFN(vkDestroyCommandPool);
    VkQueue q;
    vkGetDeviceQueue(dev, qf, 0, &q);
    VkCommandPoolCreateInfo pci = { .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
                                    .queueFamilyIndex = qf };
    VkCommandPool pool;
    VkResult r = vkCreateCommandPool(dev, &pci, NULL, &pool);
    if (r)
        return r;
    VkCommandBufferAllocateInfo cai = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
                                        .commandPool = pool,
                                        .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
                                        .commandBufferCount = 1 };
    VkCommandBuffer cb;
    vkAllocateCommandBuffers(dev, &cai, &cb);
    VkCommandBufferBeginInfo bi = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO };
    vkBeginCommandBuffer(cb, &bi);
    VkImageSubresourceRange range = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 };
    VkImageMemoryBarrier b = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
        .dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
        .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
        .newLayout = VK_IMAGE_LAYOUT_GENERAL,
        .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
        .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
        .image = image,
        .subresourceRange = range,
    };
    vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT,
                         0, 0, NULL, 0, NULL, 1, &b);
    VkClearColorValue c = { .float32 = { r_, g_, b_, 1.0f } };
    vkCmdClearColorImage(cb, image, VK_IMAGE_LAYOUT_GENERAL, &c, 1, &range);
    vkEndCommandBuffer(cb);
    VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1,
                        .pCommandBuffers = &cb };
    r = vkQueueSubmit(q, 1, &si, VK_NULL_HANDLE);
    if (!r)
        r = vkQueueWaitIdle(q);
    vkDestroyCommandPool(dev, pool, NULL);
    return r;
}

int main(int argc, char **argv)
{
    const char *path = argc > 1 ? argv[1] : "vulkan_nouveau.dll";
    const int linear = argc > 2 && !strcmp(argv[2], "linear");
    const int scanout_s = argc > 3 ? atoi(argv[3]) : 0;
    const uint32_t w = argc > 5 ? (uint32_t)atoi(argv[4]) : 1920;
    const uint32_t h = argc > 5 ? (uint32_t)atoi(argv[5]) : 1080;

    HMODULE dll = LoadLibraryExA(path, NULL, LOAD_WITH_ALTERED_SEARCH_PATH);
    if (!dll) {
        printf("LoadLibrary(%s) failed: %lu\n", path, (unsigned long)GetLastError());
        return 1;
    }
    PFN_negotiate negotiate =
        (PFN_negotiate)(void *)GetProcAddress(dll, "vk_icdNegotiateLoaderICDInterfaceVersion");
    gipa = (PFN_vkGetInstanceProcAddr)(void *)GetProcAddress(dll, "vk_icdGetInstanceProcAddr");
    PFN_helios_icd_interface_v2 helios =
        (PFN_helios_icd_interface_v2)(void *)GetProcAddress(dll, HELIOS_ICD_INTERFACE_EXPORT);
    if (!negotiate || !gipa || !helios) {
        printf("missing entry points (negotiate %p gipa %p helios %p)\n", (void *)negotiate,
               (void *)gipa, (void *)helios);
        return 1;
    }
    uint32_t version = 7;
    negotiate(&version);

    struct helios_icd_api api = { .size = sizeof(api) };
    VkResult r = helios(HELIOS_ICD_INTERFACE_VERSION, &api);
    printf("helios_icd_interface_v2: %d, version %u size %u backend %u caps 0x%x "
           "(res_id %d layout %d scanout %d producer %d)\n",
           r, api.version, api.size, api.backend, api.caps, !!(api.caps & HELIOS_ICD_CAP_RES_ID),
           !!(api.caps & HELIOS_ICD_CAP_LAYOUT), !!(api.caps & HELIOS_ICD_CAP_SCANOUT),
           !!(api.caps & HELIOS_ICD_CAP_PRODUCER));
    if (r)
        return 1;

    PFN_vkCreateInstance vkCreateInstance =
        (PFN_vkCreateInstance)gipa(VK_NULL_HANDLE, "vkCreateInstance");
    VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
                              .pApplicationName = "helios_icd_test",
                              .apiVersion = VK_API_VERSION_1_3 };
    VkInstanceCreateInfo ci = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                                .pApplicationInfo = &app };
    r = vkCreateInstance(&ci, NULL, &inst);
    if (r) {
        printf("vkCreateInstance: %d\n", r);
        return 1;
    }
    IFN(vkEnumeratePhysicalDevices);
    IFN(vkGetPhysicalDeviceProperties2);
    IFN(vkCreateDevice);
    gdpa = (PFN_vkGetDeviceProcAddr)gipa(inst, "vkGetDeviceProcAddr");

    uint32_t n = 1;
    VkPhysicalDevice pd = VK_NULL_HANDLE;
    r = vkEnumeratePhysicalDevices(inst, &n, &pd);
    if ((r != VK_SUCCESS && r != VK_INCOMPLETE) || n == 0) {
        printf("no physical device (%d)\n", r);
        return 1;
    }
    VkPhysicalDeviceIDProperties idp = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ID_PROPERTIES };
    VkPhysicalDeviceProperties2 p2 = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2,
                                       .pNext = &idp };
    vkGetPhysicalDeviceProperties2(pd, &p2);
    uint32_t luid_lo;
    int32_t luid_hi;
    memcpy(&luid_lo, idp.deviceLUID, 4);
    memcpy(&luid_hi, idp.deviceLUID + 4, 4);
    printf("device: %s, deviceLUIDValid %u LUID %08x:%08lx nodeMask %u\n",
           p2.properties.deviceName, idp.deviceLUIDValid, (unsigned)luid_hi,
           (unsigned long)luid_lo, idp.deviceNodeMask);

    float prio = 1.0f;
    VkDeviceQueueCreateInfo qci = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                                    .queueFamilyIndex = 0, .queueCount = 1,
                                    .pQueuePriorities = &prio };
    VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                               .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci };
    r = vkCreateDevice(pd, &dci, NULL, &dev);
    if (r) {
        printf("vkCreateDevice: %d\n", r);
        return 1;
    }

    struct img im[3];
    const VkImageTiling tiling = linear ? VK_IMAGE_TILING_LINEAR : VK_IMAGE_TILING_OPTIMAL;
    for (int i = 0; i < 3; i++) {
        r = make_image(pd, w, h, tiling, &im[i]);
        if (r) {
            printf("image %d: %d\n", i, r);
            return 1;
        }
        clear_image(0, im[i].image, i == 0 ? 1.0f : 0.1f, i == 1 ? 1.0f : 0.1f,
                    i == 2 ? 1.0f : 0.1f);
    }

    uint64_t sz = 0;
    uint32_t mti = 0;
    api.memory_alloc_info(im[0].mem, &sz, &mti);
    printf("memory_alloc_info: %llu B, type %u\n", (unsigned long long)sz, mti);

    struct helios_icd_layout lay;
    uint32_t res_id = 0;
    LARGE_INTEGER f, t0, t1;
    QueryPerformanceFrequency(&f);
    QueryPerformanceCounter(&t0);
    r = api.memory_res_id(dev, im[0].mem, im[0].image, &res_id, &lay);
    QueryPerformanceCounter(&t1);
    printf("memory_res_id: %d res_id %u (%.2f ms) ctx %u layout %ux%u stride %u offset %u "
           "fourcc 0x%08x modifier 0x%016llx size %llu\n",
           r, res_id, 1000.0 * (t1.QuadPart - t0.QuadPart) / f.QuadPart, api.ctx_id(inst),
           lay.width, lay.height, lay.stride, lay.offset, lay.fourcc,
           (unsigned long long)lay.modifier, (unsigned long long)lay.size);
    if (r == VK_SUCCESS) {
        uint32_t again = 0;
        api.memory_res_id(dev, im[0].mem, im[0].image, &again, NULL);
        printf("memory_res_id again: %u (cached: %s)\n", again, again == res_id ? "yes" : "NO");
    }

    if (scanout_s > 0) {
        unsigned frames = 0, failed = 0;
        const DWORD end = GetTickCount() + (DWORD)scanout_s * 1000u;
        while ((int)(end - GetTickCount()) > 0) {
            struct img *x = &im[(frames / 30) % 3];
            if (api.scanout_present(dev, x->mem, x->image) != VK_SUCCESS)
                failed++;
            frames++;
            Sleep(16);
        }
        api.scanout_release(dev);
        printf("scanout_present: %u frames, %u failed\n", frames, failed);
    }

    DFN(vkDestroyImage);
    DFN(vkFreeMemory);
    DFN(vkDestroyDevice);
    for (int i = 0; i < 3; i++) {
        vkDestroyImage(dev, im[i].image, NULL);
        vkFreeMemory(dev, im[i].mem, NULL);
    }
    vkDestroyDevice(dev, NULL);
    PFN_vkDestroyInstance vkDestroyInstance = (PFN_vkDestroyInstance)gipa(inst, "vkDestroyInstance");
    vkDestroyInstance(inst, NULL);
    printf("done\n");
    return 0;
}
