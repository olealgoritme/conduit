/*
 * Shared surfaces between two NVK processes (patch 0031, Conduit
 * guest/windows/docs/shared-surfaces.md), the way the Helios D3D11 UMD uses
 * them, without the UMD or a WDDM allocation:
 *
 *   creator: a dedicated B8G8R8A8 OPTIMAL image with the Helios export
 *            request, filled by the GPU from a pattern, its resource id and
 *            layout (memory_res_id);
 *   opener:  a second process builds the same image, imports the resource
 *            with helios_import_memory_resource_info (host RmResourceImport,
 *            then GEM_EXPORT_NVKMS_MEMORY + OS_UNIX_IMPORT_OBJECT_FROM_FD),
 *            reads it back through the GPU and checks the pattern, then the
 *            GPU writes a second pattern into the lower half;
 *   creator: reads the lower half back: the opener's GPU writes are there.
 *
 * Also: an import whose layout does not match is refused. Without a WDDM
 * allocation the KMD's open check is not exercised (and a KMD that enforces
 * "opened by this device" refuses the import: that is the KMD working).
 *
 *   helios_share_test.exe [dll] [w h]
 *
 * Run with NVK_RM=1 and librmclient.dll (with crm_win_rm_resource_import)
 * next to the driver. Exit 0 when every check passed.
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
static VkPhysicalDevice pd;
static VkDevice dev;
static VkQueue queue;
static PFN_vkGetDeviceProcAddr gdpa;
static struct helios_icd_api api = { .size = sizeof(api) };
static const char *role = "creator";
static int failed;

#define IFN(name) PFN_##name name = (PFN_##name)gipa(inst, #name)
#define DFN(name) PFN_##name name = (PFN_##name)gdpa(dev, #name)

static void check(const char *what, int ok)
{
    printf("[%s] %s: %s\n", ok ? " ok " : "FAIL", role, what);
    fflush(stdout);
    if (!ok)
        failed = 1;
}

static uint32_t find_type(uint32_t bits, VkMemoryPropertyFlags want)
{
    IFN(vkGetPhysicalDeviceMemoryProperties);
    VkPhysicalDeviceMemoryProperties mp;
    vkGetPhysicalDeviceMemoryProperties(pd, &mp);
    for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
        if ((bits & (1u << i)) && (mp.memoryTypes[i].propertyFlags & want) == want)
            return i;
    return 0;
}

static int open_driver(const char *path)
{
    HMODULE dll = LoadLibraryExA(path, NULL, LOAD_WITH_ALTERED_SEARCH_PATH);
    if (!dll) {
        printf("LoadLibrary(%s) failed: %lu\n", path, (unsigned long)GetLastError());
        return -1;
    }
    PFN_negotiate negotiate =
        (PFN_negotiate)(void *)GetProcAddress(dll, "vk_icdNegotiateLoaderICDInterfaceVersion");
    gipa = (PFN_vkGetInstanceProcAddr)(void *)GetProcAddress(dll, "vk_icdGetInstanceProcAddr");
    PFN_helios_icd_interface_v2 helios =
        (PFN_helios_icd_interface_v2)(void *)GetProcAddress(dll, HELIOS_ICD_INTERFACE_EXPORT);
    if (!negotiate || !gipa || !helios)
        return -1;
    uint32_t version = 7;
    negotiate(&version);
    if (helios(HELIOS_ICD_INTERFACE_VERSION, &api) != VK_SUCCESS)
        return -1;
    printf("       %s: interface size %u caps 0x%x (res_id %d, shared_import %d)\n", role,
           api.size, api.caps, !!(api.caps & HELIOS_ICD_CAP_RES_ID),
           !!(api.caps & HELIOS_ICD_CAP_SHARED_IMPORT));

    PFN_vkCreateInstance vkCreateInstance =
        (PFN_vkCreateInstance)gipa(VK_NULL_HANDLE, "vkCreateInstance");
    VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
                              .pApplicationName = "helios_share_test",
                              .apiVersion = VK_API_VERSION_1_3 };
    VkInstanceCreateInfo ci = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                                .pApplicationInfo = &app };
    if (vkCreateInstance(&ci, NULL, &inst))
        return -1;
    IFN(vkEnumeratePhysicalDevices);
    IFN(vkCreateDevice);
    gdpa = (PFN_vkGetDeviceProcAddr)gipa(inst, "vkGetDeviceProcAddr");
    uint32_t n = 1;
    VkResult r = vkEnumeratePhysicalDevices(inst, &n, &pd);
    if ((r != VK_SUCCESS && r != VK_INCOMPLETE) || n == 0)
        return -1;
    float prio = 1.0f;
    VkDeviceQueueCreateInfo qci = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                                    .queueFamilyIndex = 0, .queueCount = 1,
                                    .pQueuePriorities = &prio };
    VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                               .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci };
    if (vkCreateDevice(pd, &dci, NULL, &dev))
        return -1;
    DFN(vkGetDeviceQueue);
    vkGetDeviceQueue(dev, 0, 0, &queue);
    return 0;
}

static VkResult create_image(uint32_t w, uint32_t h, VkImage *image)
{
    DFN(vkCreateImage);
    VkImageCreateInfo ici = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
        .imageType = VK_IMAGE_TYPE_2D,
        .format = VK_FORMAT_B8G8R8A8_UNORM,
        .extent = { w, h, 1 },
        .mipLevels = 1,
        .arrayLayers = 1,
        .samples = VK_SAMPLE_COUNT_1_BIT,
        .tiling = VK_IMAGE_TILING_OPTIMAL,
        .usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT |
                 VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_SAMPLED_BIT,
        .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
    };
    return vkCreateImage(dev, &ici, NULL, image);
}

/* Dedicated memory for `image`: exported (creator) or imported (opener) */
static VkResult bind_memory(VkImage image, const void *pnext, VkDeviceMemory *mem)
{
    DFN(vkGetImageMemoryRequirements);
    DFN(vkAllocateMemory);
    DFN(vkBindImageMemory);
    VkMemoryRequirements req;
    vkGetImageMemoryRequirements(dev, image, &req);
    VkMemoryDedicatedAllocateInfo ded = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
        .pNext = pnext,
        .image = image,
    };
    VkMemoryAllocateInfo mai = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        .pNext = &ded,
        .allocationSize = req.size,
        .memoryTypeIndex = find_type(req.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT),
    };
    VkResult r = vkAllocateMemory(dev, &mai, NULL, mem);
    if (r)
        return r;
    return vkBindImageMemory(dev, image, *mem, 0);
}

struct hostbuf {
    VkBuffer buf;
    VkDeviceMemory mem;
    uint32_t *map;
};

static VkResult make_hostbuf(VkDeviceSize size, struct hostbuf *b)
{
    DFN(vkCreateBuffer);
    DFN(vkGetBufferMemoryRequirements);
    DFN(vkAllocateMemory);
    DFN(vkBindBufferMemory);
    DFN(vkMapMemory);
    VkBufferCreateInfo bci = { .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size = size,
                               .usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT |
                                        VK_BUFFER_USAGE_TRANSFER_DST_BIT };
    VkResult r = vkCreateBuffer(dev, &bci, NULL, &b->buf);
    if (r)
        return r;
    VkMemoryRequirements req;
    vkGetBufferMemoryRequirements(dev, b->buf, &req);
    VkMemoryAllocateInfo mai = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        .allocationSize = req.size,
        .memoryTypeIndex = find_type(req.memoryTypeBits, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                                                         VK_MEMORY_PROPERTY_HOST_COHERENT_BIT),
    };
    if ((r = vkAllocateMemory(dev, &mai, NULL, &b->mem)))
        return r;
    if ((r = vkBindBufferMemory(dev, b->buf, b->mem, 0)))
        return r;
    return vkMapMemory(dev, b->mem, 0, VK_WHOLE_SIZE, 0, (void **)&b->map);
}

/* One copy between the image (rows y0..y1) and the buffer, waited for */
static VkResult copy(VkImage image, struct hostbuf *b, uint32_t w, uint32_t y0, uint32_t rows,
                     int to_image, VkImageLayout *layout)
{
    DFN(vkCreateCommandPool);
    DFN(vkAllocateCommandBuffers);
    DFN(vkBeginCommandBuffer);
    DFN(vkCmdPipelineBarrier);
    DFN(vkCmdCopyBufferToImage);
    DFN(vkCmdCopyImageToBuffer);
    DFN(vkEndCommandBuffer);
    DFN(vkQueueSubmit);
    DFN(vkQueueWaitIdle);
    DFN(vkDestroyCommandPool);
    VkCommandPoolCreateInfo pci = { .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO };
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
    VkImageMemoryBarrier bar = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
        .srcAccessMask = VK_ACCESS_MEMORY_WRITE_BIT,
        .dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT | VK_ACCESS_TRANSFER_READ_BIT,
        .oldLayout = *layout,
        .newLayout = VK_IMAGE_LAYOUT_GENERAL,
        .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
        .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
        .image = image,
        .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 },
    };
    vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT,
                         0, 0, NULL, 0, NULL, 1, &bar);
    *layout = VK_IMAGE_LAYOUT_GENERAL;
    VkBufferImageCopy region = {
        .bufferOffset = (VkDeviceSize)y0 * w * 4,
        .imageSubresource = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1 },
        .imageOffset = { 0, (int32_t)y0, 0 },
        .imageExtent = { w, rows, 1 },
    };
    if (to_image)
        vkCmdCopyBufferToImage(cb, b->buf, image, VK_IMAGE_LAYOUT_GENERAL, 1, &region);
    else
        vkCmdCopyImageToBuffer(cb, image, VK_IMAGE_LAYOUT_GENERAL, b->buf, 1, &region);
    vkEndCommandBuffer(cb);
    VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1,
                        .pCommandBuffers = &cb };
    r = vkQueueSubmit(queue, 1, &si, VK_NULL_HANDLE);
    if (!r)
        r = vkQueueWaitIdle(queue);
    vkDestroyCommandPool(dev, pool, NULL);
    return r;
}

/* A pattern every texel of which says where it is: a layout mismatch
 * (pitch, block height, offset) cannot pass for the right one. */
static uint32_t texel(uint32_t x, uint32_t y, uint32_t seed)
{
    return ((x * 7919u) ^ (y * 104729u) ^ seed) | 0xff000000u;
}

static size_t count_bad(const uint32_t *p, uint32_t w, uint32_t y0, uint32_t rows, uint32_t seed)
{
    size_t bad = 0;
    for (uint32_t y = y0; y < y0 + rows; y++)
        for (uint32_t x = 0; x < w; x++)
            bad += p[(size_t)y * w + x] != texel(x, y, seed);
    return bad;
}

static int run_open(int argc, char **argv)
{
    role = "opener";
    /* dll w h resid size modifier stride offset */
    if (argc < 10 || open_driver(argv[2])) {
        check("open the driver", 0);
        return 1;
    }
    const uint32_t w = (uint32_t)strtoul(argv[3], NULL, 0);
    const uint32_t h = (uint32_t)strtoul(argv[4], NULL, 0);
    struct helios_import_memory_resource_info imp = {
        .sType = HELIOS_STRUCTURE_TYPE_IMPORT_MEMORY_RESOURCE_INFO,
        .resource_id = (uint32_t)strtoul(argv[5], NULL, 0),
        .size = strtoull(argv[6], NULL, 0),
        .modifier = strtoull(argv[7], NULL, 0),
        .stride = (uint32_t)strtoul(argv[8], NULL, 0),
        .offset = (uint32_t)strtoul(argv[9], NULL, 0),
    };
    DFN(vkDestroyImage);
    DFN(vkFreeMemory);

    /* Wrong layout: refused, nothing duplicated */
    VkImage image = VK_NULL_HANDLE;
    VkDeviceMemory mem = VK_NULL_HANDLE;
    struct helios_import_memory_resource_info bad = imp;
    bad.stride += 64;
    create_image(w, h, &image);
    VkResult r = bind_memory(image, &bad, &mem);
    printf("       opener: import with a wrong row pitch: %d\n", r);
    check("an import whose layout differs from the creator's is refused",
          r == VK_ERROR_INVALID_EXTERNAL_HANDLE);
    if (mem)
        vkFreeMemory(dev, mem, NULL);
    vkDestroyImage(dev, image, NULL);

    image = VK_NULL_HANDLE;
    mem = VK_NULL_HANDLE;
    if (create_image(w, h, &image) || (r = bind_memory(image, &imp, &mem))) {
        printf("       opener: import: %d\n", r);
        check("import the creator's memory by resource id", 0);
        return 1;
    }
    check("import the creator's memory by resource id", 1);
    uint32_t again = 0;
    if (api.memory_res_id) {
        api.memory_res_id(dev, mem, image, &again, NULL);
        check("the imported memory has the creator's resource id", again == imp.resource_id);
    }

    struct hostbuf hb;
    VkImageLayout layout = VK_IMAGE_LAYOUT_UNDEFINED;
    if (make_hostbuf((VkDeviceSize)w * h * 4, &hb)) {
        check("host buffer", 0);
        return 1;
    }
    memset(hb.map, 0, (size_t)w * h * 4);
    r = copy(image, &hb, w, 0, h, 0, &layout);
    size_t nbad = count_bad(hb.map, w, 0, h, 0x00a5c3e1u);
    printf("       opener: read back %d, %zu of %u texels differ\n", r, nbad, w * h);
    check("the GPU reads the creator's texels through the imported memory", r == 0 && nbad == 0);

    for (uint32_t y = h / 2; y < h; y++)
        for (uint32_t x = 0; x < w; x++)
            hb.map[(size_t)y * w + x] = texel(x, y, 0x003c5a77u);
    r = copy(image, &hb, w, h / 2, h - h / 2, 1, &layout);
    check("the GPU writes the lower half through the imported memory", r == 0);

    vkDestroyImage(dev, image, NULL);
    vkFreeMemory(dev, mem, NULL);
    DFN(vkDestroyDevice);
    vkDestroyDevice(dev, NULL);
    printf("%s\n", failed ? "OPENER FAILED" : "OPENER PASSED");
    return failed;
}

int main(int argc, char **argv)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    if (argc > 1 && !strcmp(argv[1], "open"))
        return run_open(argc, argv);

    const char *path = argc > 1 ? argv[1] : "vulkan_nouveau.dll";
    const uint32_t w = argc > 3 ? (uint32_t)atoi(argv[2]) : 1920;
    const uint32_t h = argc > 3 ? (uint32_t)atoi(argv[3]) : 1080;
    if (open_driver(path)) {
        check("open the driver", 0);
        return 1;
    }
    if (!(api.caps & HELIOS_ICD_CAP_SHARED_IMPORT)) {
        check("the driver opens shared surfaces (HELIOS_ICD_CAP_SHARED_IMPORT)", 0);
        return 1;
    }
    DFN(vkDestroyImage);
    DFN(vkFreeMemory);

    VkImage image;
    VkDeviceMemory mem;
    struct helios_export_memory_resource_info hx = {
        .sType = HELIOS_STRUCTURE_TYPE_EXPORT_MEMORY_RESOURCE_INFO,
    };
    VkResult r = create_image(w, h, &image);
    if (!r)
        r = bind_memory(image, &hx, &mem);
    check("a dedicated exportable image", r == 0);
    if (r)
        return 1;

    struct hostbuf hb;
    VkImageLayout layout = VK_IMAGE_LAYOUT_UNDEFINED;
    if (make_hostbuf((VkDeviceSize)w * h * 4, &hb)) {
        check("host buffer", 0);
        return 1;
    }
    for (uint32_t y = 0; y < h; y++)
        for (uint32_t x = 0; x < w; x++)
            hb.map[(size_t)y * w + x] = texel(x, y, 0x00a5c3e1u);
    check("the GPU fills it", copy(image, &hb, w, 0, h, 1, &layout) == 0);

    uint32_t res_id = 0;
    struct helios_icd_layout lay = { 0 };
    r = api.memory_res_id(dev, mem, image, &res_id, &lay);
    printf("       creator: memory_res_id %d: resource %u, %ux%u stride %u offset %u modifier "
           "0x%016llx\n", r, res_id, lay.width, lay.height, lay.stride, lay.offset,
           (unsigned long long)lay.modifier);
    check("a resource id with its layout (IMPORT_RM)", r == 0 && res_id != 0);
    if (r)
        return 1;

    char self[MAX_PATH], cmd[1024];
    GetModuleFileNameA(NULL, self, sizeof(self));
    snprintf(cmd, sizeof(cmd),
             "\"%s\" open \"%s\" %u %u %u %llu 0x%016llx %u %u", self, path, w, h, res_id,
             (unsigned long long)lay.size, (unsigned long long)lay.modifier, lay.stride,
             lay.offset);
    STARTUPINFOA si = { .cb = sizeof(si) };
    PROCESS_INFORMATION pi;
    if (!CreateProcessA(NULL, cmd, NULL, NULL, TRUE, 0, NULL, NULL, &si, &pi)) {
        check("start the opener", 0);
        return 1;
    }
    WaitForSingleObject(pi.hProcess, 120000);
    DWORD code = 1;
    GetExitCodeProcess(pi.hProcess, &code);
    CloseHandle(pi.hProcess);
    CloseHandle(pi.hThread);
    check("the opener passed", code == 0);

    memset(hb.map, 0, (size_t)w * h * 4);
    r = copy(image, &hb, w, 0, h, 0, &layout);
    size_t top = count_bad(hb.map, w, 0, h / 2, 0x00a5c3e1u);
    size_t bottom = count_bad(hb.map, w, h / 2, h - h / 2, 0x003c5a77u);
    printf("       creator: read back %d: %zu texels differ above, %zu below\n", r, top, bottom);
    check("the upper half is still ours", r == 0 && top == 0);
    check("the lower half is what the opener's GPU wrote (same memory)", r == 0 && bottom == 0);

    vkDestroyImage(dev, image, NULL);
    vkFreeMemory(dev, mem, NULL);
    DFN(vkDestroyDevice);
    vkDestroyDevice(dev, NULL);
    printf("%s\n", failed ? "SHARE TEST FAILED" : "SHARE TEST PASSED");
    return failed;
}
