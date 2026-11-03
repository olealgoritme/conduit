/*
 * vk_memory_probe: what a Vulkan ICD reports about memory, and whether large
 * device-local images can actually get memory.
 *
 * Prints, for the NVIDIA physical device (vendor 0x10DE; --any for the first
 * device): driver/API versions, the allocation limits (maxMemoryAllocationCount,
 * maxMemoryAllocationSize, bufferImageGranularity), every memory heap and type,
 * and the VK_EXT_memory_budget budget/usage per heap when the extension exists.
 * Then, on a device created with VK_EXT_memory_budget when available:
 *
 *   images   a set of render-target / texture shapes like a game's (5120x1440
 *            colour/depth, HDR, MSAA, BC7 with mips, arrays, a 3D volume,
 *            16384^2): vkCreateImage, vkGetImageMemoryRequirements2 (size,
 *            alignment, memoryTypeBits, prefers/requires dedicated), then
 *            vkAllocateMemory from the first DEVICE_LOCAL type in the bits
 *            (dedicated when preferred) and vkBindImageMemory; every result.
 *   sizes    single DEVICE_LOCAL allocations of 64 MiB .. 8 GiB.
 *   fill     DEVICE_LOCAL allocations of 512 MiB kept until one fails or
 *            --fill-mb (default 12288) is reached: where the heap really ends.
 *   budget   the heap budgets again after the fill (before freeing).
 *
 * Every line is flushed. A watchdog ends the process after 120 s.
 * Exit code 0 when every image got memory, 1 otherwise, 2 on setup failure.
 *
 * Build (host, MinGW-w64; vulkan-1.dll is loaded at run time):
 *   x86_64-w64-mingw32-gcc -O2 -Wall -I<dir with vulkan/vulkan_core.h> \
 *       -o vk_memory_probe.exe vk_memory_probe.c -static
 * Run (any session; no window): vk_memory_probe.exe [--any] [--fill-mb N]
 */
#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#include <windows.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define VK_NO_PROTOTYPES
#include <vulkan/vulkan_core.h>

#define NV_VENDOR_ID 0x10DEu

static void out(const char *fmt, ...)
{
    va_list ap;
    va_start(ap, fmt);
    vprintf(fmt, ap);
    va_end(ap);
    fflush(stdout);
}

static DWORD WINAPI watchdog(LPVOID arg)
{
    (void)arg;
    Sleep(120000);
    out("FAIL: watchdog (120 s)\n");
    ExitProcess(3);
    return 0;
}

static const char *res(VkResult r)
{
    static char buf[32];
    switch (r) {
    case VK_SUCCESS: return "VK_SUCCESS";
    case VK_ERROR_OUT_OF_HOST_MEMORY: return "OUT_OF_HOST_MEMORY";
    case VK_ERROR_OUT_OF_DEVICE_MEMORY: return "OUT_OF_DEVICE_MEMORY";
    case VK_ERROR_INITIALIZATION_FAILED: return "INITIALIZATION_FAILED";
    case VK_ERROR_DEVICE_LOST: return "DEVICE_LOST";
    case VK_ERROR_FEATURE_NOT_PRESENT: return "FEATURE_NOT_PRESENT";
    case VK_ERROR_FORMAT_NOT_SUPPORTED: return "FORMAT_NOT_SUPPORTED";
    case VK_ERROR_TOO_MANY_OBJECTS: return "TOO_MANY_OBJECTS";
    case VK_ERROR_INVALID_EXTERNAL_HANDLE: return "INVALID_EXTERNAL_HANDLE";
    case VK_ERROR_EXTENSION_NOT_PRESENT: return "EXTENSION_NOT_PRESENT";
    default: snprintf(buf, sizeof buf, "VkResult %d", (int)r); return buf;
    }
}

#define FN(name) static PFN_##name name
FN(vkGetInstanceProcAddr);
FN(vkCreateInstance);
FN(vkEnumeratePhysicalDevices);
FN(vkGetPhysicalDeviceProperties2);
FN(vkGetPhysicalDeviceMemoryProperties2);
FN(vkGetPhysicalDeviceQueueFamilyProperties);
FN(vkGetPhysicalDeviceImageFormatProperties);
FN(vkEnumerateDeviceExtensionProperties);
FN(vkCreateDevice);
FN(vkCreateImage);
FN(vkDestroyImage);
FN(vkGetImageMemoryRequirements2);
FN(vkAllocateMemory);
FN(vkFreeMemory);
FN(vkBindImageMemory);
FN(vkDestroyDevice);
FN(vkDestroyInstance);

static VkPhysicalDevice phys;
static VkDevice dev;
static VkPhysicalDeviceMemoryProperties mem;
static int has_budget;

static void print_budget(const char *when)
{
    VkPhysicalDeviceMemoryBudgetPropertiesEXT budget = {
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_MEMORY_BUDGET_PROPERTIES_EXT};
    VkPhysicalDeviceMemoryProperties2 p2 = {
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_MEMORY_PROPERTIES_2,
        .pNext = has_budget ? &budget : NULL};
    vkGetPhysicalDeviceMemoryProperties2(phys, &p2);
    mem = p2.memoryProperties;
    if (!has_budget)
        return;
    for (uint32_t i = 0; i < mem.memoryHeapCount; i++)
        out("budget %-6s heap %u: budget %8llu MiB  usage %8llu MiB\n", when, i,
            (unsigned long long)(budget.heapBudget[i] >> 20),
            (unsigned long long)(budget.heapUsage[i] >> 20));
}

static void print_flags(VkMemoryPropertyFlags f)
{
    if (f & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT) out(" DEVICE_LOCAL");
    if (f & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT) out(" HOST_VISIBLE");
    if (f & VK_MEMORY_PROPERTY_HOST_COHERENT_BIT) out(" HOST_COHERENT");
    if (f & VK_MEMORY_PROPERTY_HOST_CACHED_BIT) out(" HOST_CACHED");
    if (f & VK_MEMORY_PROPERTY_LAZILY_ALLOCATED_BIT) out(" LAZY");
    if (f & VK_MEMORY_PROPERTY_PROTECTED_BIT) out(" PROTECTED");
    if (f & ~0x3fu) out(" other=0x%x", f & ~0x3fu);
}

static int device_local_type(uint32_t bits, VkMemoryPropertyFlags avoid)
{
    for (uint32_t i = 0; i < mem.memoryTypeCount; i++)
        if ((bits & (1u << i)) &&
            (mem.memoryTypes[i].propertyFlags & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT) &&
            !(mem.memoryTypes[i].propertyFlags & avoid))
            return (int)i;
    for (uint32_t i = 0; i < mem.memoryTypeCount; i++)
        if ((bits & (1u << i)) &&
            (mem.memoryTypes[i].propertyFlags & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT))
            return (int)i;
    return -1;
}

struct shape {
    const char *name;
    VkImageType type;
    VkFormat format;
    uint32_t w, h, d, mips, layers;
    VkSampleCountFlagBits samples;
    VkImageUsageFlags usage;
};

#define RT (VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_SAMPLED_BIT | VK_IMAGE_USAGE_STORAGE_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT)
#define RT_NS (VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_SAMPLED_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT)
#define DS (VK_IMAGE_USAGE_DEPTH_STENCIL_ATTACHMENT_BIT | VK_IMAGE_USAGE_SAMPLED_BIT)
#define TEX (VK_IMAGE_USAGE_SAMPLED_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT)

static const struct shape shapes[] = {
    {"rgba8 1920x1080", VK_IMAGE_TYPE_2D, VK_FORMAT_R8G8B8A8_UNORM, 1920, 1080, 1, 1, 1, VK_SAMPLE_COUNT_1_BIT, RT},
    {"rgba8 5120x1440", VK_IMAGE_TYPE_2D, VK_FORMAT_R8G8B8A8_UNORM, 5120, 1440, 1, 1, 1, VK_SAMPLE_COUNT_1_BIT, RT},
    {"rgba16f 5120x1440", VK_IMAGE_TYPE_2D, VK_FORMAT_R16G16B16A16_SFLOAT, 5120, 1440, 1, 1, 1, VK_SAMPLE_COUNT_1_BIT, RT},
    {"rgba32f 5120x1440", VK_IMAGE_TYPE_2D, VK_FORMAT_R32G32B32A32_SFLOAT, 5120, 1440, 1, 1, 1, VK_SAMPLE_COUNT_1_BIT, RT},
    {"b10g11r11 5120x1440", VK_IMAGE_TYPE_2D, VK_FORMAT_B10G11R11_UFLOAT_PACK32, 5120, 1440, 1, 1, 1, VK_SAMPLE_COUNT_1_BIT, RT},
    {"rgba16f 3840x2160 rt-only", VK_IMAGE_TYPE_2D, VK_FORMAT_R16G16B16A16_SFLOAT, 3840, 2160, 1, 1, 1, VK_SAMPLE_COUNT_1_BIT, RT_NS},
    {"d32 5120x1440", VK_IMAGE_TYPE_2D, VK_FORMAT_D32_SFLOAT, 5120, 1440, 1, 1, 1, VK_SAMPLE_COUNT_1_BIT, DS},
    {"d32s8 5120x1440", VK_IMAGE_TYPE_2D, VK_FORMAT_D32_SFLOAT_S8_UINT, 5120, 1440, 1, 1, 1, VK_SAMPLE_COUNT_1_BIT, DS},
    {"d24s8 3840x2160", VK_IMAGE_TYPE_2D, VK_FORMAT_D24_UNORM_S8_UINT, 3840, 2160, 1, 1, 1, VK_SAMPLE_COUNT_1_BIT, DS},
    {"rgba8 msaa4 3840x2160", VK_IMAGE_TYPE_2D, VK_FORMAT_R8G8B8A8_UNORM, 3840, 2160, 1, 1, 1, VK_SAMPLE_COUNT_4_BIT, RT_NS},
    {"d32 msaa4 3840x2160", VK_IMAGE_TYPE_2D, VK_FORMAT_D32_SFLOAT, 3840, 2160, 1, 1, 1, VK_SAMPLE_COUNT_4_BIT, DS},
    {"bc7 4096^2 mips", VK_IMAGE_TYPE_2D, VK_FORMAT_BC7_UNORM_BLOCK, 4096, 4096, 1, 13, 1, VK_SAMPLE_COUNT_1_BIT, TEX},
    {"bc1 8192^2 mips", VK_IMAGE_TYPE_2D, VK_FORMAT_BC1_RGB_UNORM_BLOCK, 8192, 8192, 1, 14, 1, VK_SAMPLE_COUNT_1_BIT, TEX},
    {"rgba8 2048^2 x64 array", VK_IMAGE_TYPE_2D, VK_FORMAT_R8G8B8A8_UNORM, 2048, 2048, 1, 12, 64, VK_SAMPLE_COUNT_1_BIT, TEX},
    {"d32 4096^2 x4 shadow", VK_IMAGE_TYPE_2D, VK_FORMAT_D32_SFLOAT, 4096, 4096, 1, 1, 4, VK_SAMPLE_COUNT_1_BIT, DS},
    {"rgba16f 256^3 volume", VK_IMAGE_TYPE_3D, VK_FORMAT_R16G16B16A16_SFLOAT, 256, 256, 256, 1, 1, VK_SAMPLE_COUNT_1_BIT, TEX | VK_IMAGE_USAGE_STORAGE_BIT},
    {"r32f 512^3 volume", VK_IMAGE_TYPE_3D, VK_FORMAT_R32_SFLOAT, 512, 512, 512, 1, 1, VK_SAMPLE_COUNT_1_BIT, TEX | VK_IMAGE_USAGE_STORAGE_BIT},
    {"rgba8 16384^2", VK_IMAGE_TYPE_2D, VK_FORMAT_R8G8B8A8_UNORM, 16384, 16384, 1, 1, 1, VK_SAMPLE_COUNT_1_BIT, TEX},
    {"rgba16f 16384^2", VK_IMAGE_TYPE_2D, VK_FORMAT_R16G16B16A16_SFLOAT, 16384, 16384, 1, 1, 1, VK_SAMPLE_COUNT_1_BIT, TEX},
};

static int try_image(const struct shape *s)
{
    VkImageCreateInfo ci = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
        .imageType = s->type,
        .format = s->format,
        .extent = {s->w, s->h, s->d},
        .mipLevels = s->mips,
        .arrayLayers = s->layers,
        .samples = s->samples,
        .tiling = VK_IMAGE_TILING_OPTIMAL,
        .usage = s->usage,
        .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
        .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
    };
    VkImageFormatProperties fp;
    VkResult r = vkGetPhysicalDeviceImageFormatProperties(phys, s->format, s->type, ci.tiling, s->usage, 0, &fp);
    if (r != VK_SUCCESS) {
        out("image %-26s format/usage not supported (%s): skipped\n", s->name, res(r));
        return 0;
    }
    VkImage img;
    r = vkCreateImage(dev, &ci, NULL, &img);
    if (r != VK_SUCCESS) {
        out("image %-26s vkCreateImage %s\n", s->name, res(r));
        return 1;
    }
    VkImageMemoryRequirementsInfo2 ri = {.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_REQUIREMENTS_INFO_2, .image = img};
    VkMemoryDedicatedRequirements dr = {.sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_REQUIREMENTS};
    VkMemoryRequirements2 mr = {.sType = VK_STRUCTURE_TYPE_MEMORY_REQUIREMENTS_2, .pNext = &dr};
    vkGetImageMemoryRequirements2(dev, &ri, &mr);
    VkMemoryRequirements m = mr.memoryRequirements;
    int type = device_local_type(m.memoryTypeBits, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT);
    out("image %-26s size %10llu (%5llu MiB) align 0x%llx bits 0x%x prefersDed %u requiresDed %u type %d",
        s->name, (unsigned long long)m.size, (unsigned long long)(m.size >> 20),
        (unsigned long long)m.alignment, m.memoryTypeBits, dr.prefersDedicatedAllocation,
        dr.requiresDedicatedAllocation, type);
    int bad = 0;
    if (type < 0) {
        out(" -> NO DEVICE_LOCAL TYPE IN BITS\n");
        bad = 1;
    } else {
        VkMemoryDedicatedAllocateInfo dai = {.sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO, .image = img};
        int dedicated = dr.prefersDedicatedAllocation || dr.requiresDedicatedAllocation || m.size >= (64ull << 20);
        VkMemoryAllocateInfo ai = {
            .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
            .pNext = dedicated ? &dai : NULL,
            .allocationSize = m.size,
            .memoryTypeIndex = (uint32_t)type,
        };
        VkDeviceMemory dm;
        r = vkAllocateMemory(dev, &ai, NULL, &dm);
        out(" alloc%s %s", dedicated ? "(dedicated)" : "", res(r));
        if (r == VK_SUCCESS) {
            VkResult b = vkBindImageMemory(dev, img, dm, 0);
            out(" bind %s", res(b));
            bad = b != VK_SUCCESS;
            vkFreeMemory(dev, dm, NULL);
        } else {
            bad = 1;
            if (dedicated) {
                ai.pNext = NULL;
                r = vkAllocateMemory(dev, &ai, NULL, &dm);
                out(" | plain alloc %s", res(r));
                if (r == VK_SUCCESS)
                    vkFreeMemory(dev, dm, NULL);
            }
        }
        out("\n");
    }
    vkDestroyImage(dev, img, NULL);
    return bad;
}

static int plain_alloc(uint64_t bytes, VkDeviceMemory *keep)
{
    int type = device_local_type(~0u, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT);
    if (type < 0)
        return 0;
    VkMemoryAllocateInfo ai = {.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = bytes, .memoryTypeIndex = (uint32_t)type};
    VkDeviceMemory dm;
    VkResult r = vkAllocateMemory(dev, &ai, NULL, &dm);
    if (keep) {
        *keep = r == VK_SUCCESS ? dm : VK_NULL_HANDLE;
        return r == VK_SUCCESS;
    }
    out("alloc %6llu MiB DEVICE_LOCAL type %d: %s\n", (unsigned long long)(bytes >> 20), type, res(r));
    if (r == VK_SUCCESS)
        vkFreeMemory(dev, dm, NULL);
    return r == VK_SUCCESS;
}

int main(int argc, char **argv)
{
    int any = 0;
    uint64_t fill_mb = 12288;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--any"))
            any = 1;
        else if (!strcmp(argv[i], "--fill-mb") && i + 1 < argc)
            fill_mb = strtoull(argv[++i], NULL, 0);
    }
    CreateThread(NULL, 0, watchdog, NULL, 0, NULL);
    HMODULE lib = LoadLibraryA("vulkan-1.dll");
    if (!lib) {
        out("FAIL: no vulkan-1.dll\n");
        return 2;
    }
    vkGetInstanceProcAddr = (PFN_vkGetInstanceProcAddr)(void (*)(void))GetProcAddress(lib, "vkGetInstanceProcAddr");
    vkCreateInstance = (PFN_vkCreateInstance)vkGetInstanceProcAddr(NULL, "vkCreateInstance");
    VkApplicationInfo app = {.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO, .pApplicationName = "vk_memory_probe", .apiVersion = VK_API_VERSION_1_2};
    VkInstanceCreateInfo ici = {.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app};
    VkInstance inst;
    VkResult r = vkCreateInstance(&ici, NULL, &inst);
    if (r != VK_SUCCESS) {
        out("FAIL: vkCreateInstance %s\n", res(r));
        return 2;
    }
#define IFN(name) name = (PFN_##name)vkGetInstanceProcAddr(inst, #name)
    IFN(vkEnumeratePhysicalDevices);
    IFN(vkGetPhysicalDeviceProperties2);
    IFN(vkGetPhysicalDeviceMemoryProperties2);
    IFN(vkGetPhysicalDeviceQueueFamilyProperties);
    IFN(vkGetPhysicalDeviceImageFormatProperties);
    IFN(vkEnumerateDeviceExtensionProperties);
    IFN(vkCreateDevice);
    IFN(vkDestroyInstance);
    uint32_t n = 0;
    vkEnumeratePhysicalDevices(inst, &n, NULL);
    VkPhysicalDevice list[16];
    if (n > 16)
        n = 16;
    vkEnumeratePhysicalDevices(inst, &n, list);
    phys = VK_NULL_HANDLE;
    for (uint32_t i = 0; i < n; i++) {
        VkPhysicalDeviceProperties2 p = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2};
        vkGetPhysicalDeviceProperties2(list[i], &p);
        out("device %u: %s vendor 0x%04x device 0x%04x driver 0x%08x api %u.%u.%u\n", i,
            p.properties.deviceName, p.properties.vendorID, p.properties.deviceID,
            p.properties.driverVersion, VK_API_VERSION_MAJOR(p.properties.apiVersion),
            VK_API_VERSION_MINOR(p.properties.apiVersion), VK_API_VERSION_PATCH(p.properties.apiVersion));
        if (!phys && (any || p.properties.vendorID == NV_VENDOR_ID))
            phys = list[i];
    }
    if (!phys) {
        out("FAIL: no NVIDIA physical device\n");
        return 2;
    }
    VkPhysicalDeviceMaintenance3Properties m3 = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_MAINTENANCE_3_PROPERTIES};
    VkPhysicalDeviceProperties2 p = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2, .pNext = &m3};
    vkGetPhysicalDeviceProperties2(phys, &p);
    out("limits: maxMemoryAllocationCount %u maxMemoryAllocationSize %llu MiB bufferImageGranularity 0x%llx maxImageDimension2D %u maxImageDimension3D %u sparseBinding %u\n",
        p.properties.limits.maxMemoryAllocationCount, (unsigned long long)(m3.maxMemoryAllocationSize >> 20),
        (unsigned long long)p.properties.limits.bufferImageGranularity, p.properties.limits.maxImageDimension2D,
        p.properties.limits.maxImageDimension3D, p.properties.limits.sparseAddressSpaceSize != 0);

    uint32_t ne = 0;
    vkEnumerateDeviceExtensionProperties(phys, NULL, &ne, NULL);
    VkExtensionProperties *ext = calloc(ne ? ne : 1, sizeof *ext);
    vkEnumerateDeviceExtensionProperties(phys, NULL, &ne, ext);
    for (uint32_t i = 0; i < ne; i++)
        if (!strcmp(ext[i].extensionName, VK_EXT_MEMORY_BUDGET_EXTENSION_NAME))
            has_budget = 1;
    out("VK_EXT_memory_budget: %s\n", has_budget ? "yes" : "no");

    print_budget("start");
    for (uint32_t i = 0; i < mem.memoryHeapCount; i++) {
        out("heap %u: %8llu MiB flags 0x%x%s\n", i, (unsigned long long)(mem.memoryHeaps[i].size >> 20),
            mem.memoryHeaps[i].flags,
            (mem.memoryHeaps[i].flags & VK_MEMORY_HEAP_DEVICE_LOCAL_BIT) ? " DEVICE_LOCAL" : "");
    }
    for (uint32_t i = 0; i < mem.memoryTypeCount; i++) {
        out("type %2u: heap %u flags 0x%03x", i, mem.memoryTypes[i].heapIndex, mem.memoryTypes[i].propertyFlags);
        print_flags(mem.memoryTypes[i].propertyFlags);
        out("\n");
    }

    uint32_t nq = 0;
    vkGetPhysicalDeviceQueueFamilyProperties(phys, &nq, NULL);
    float prio = 1.0f;
    VkDeviceQueueCreateInfo qci = {.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueFamilyIndex = 0, .queueCount = 1, .pQueuePriorities = &prio};
    const char *exts[] = {VK_EXT_MEMORY_BUDGET_EXTENSION_NAME};
    VkDeviceCreateInfo dci = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
        .queueCreateInfoCount = 1,
        .pQueueCreateInfos = &qci,
        .enabledExtensionCount = has_budget ? 1u : 0u,
        .ppEnabledExtensionNames = exts,
    };
    r = vkCreateDevice(phys, &dci, NULL, &dev);
    if (r != VK_SUCCESS) {
        out("FAIL: vkCreateDevice %s\n", res(r));
        return 2;
    }
    PFN_vkGetDeviceProcAddr gdpa = (PFN_vkGetDeviceProcAddr)vkGetInstanceProcAddr(inst, "vkGetDeviceProcAddr");
#define DFN(name) name = (PFN_##name)gdpa(dev, #name)
    DFN(vkCreateImage);
    DFN(vkDestroyImage);
    DFN(vkGetImageMemoryRequirements2);
    DFN(vkAllocateMemory);
    DFN(vkFreeMemory);
    DFN(vkBindImageMemory);
    DFN(vkDestroyDevice);

    int failed = 0;
    for (size_t i = 0; i < sizeof shapes / sizeof shapes[0]; i++)
        failed += try_image(&shapes[i]);

    for (uint64_t mb = 64; mb <= 8192; mb *= 2)
        plain_alloc(mb << 20, NULL);

    enum { MAXKEEP = 256 };
    static VkDeviceMemory keep[MAXKEEP];
    uint32_t kept = 0;
    while (kept < MAXKEEP && (uint64_t)kept * 512 < fill_mb && plain_alloc(512ull << 20, &keep[kept]))
        kept++;
    out("fill: %u x 512 MiB DEVICE_LOCAL allocations (%llu MiB) before %s\n", kept,
        (unsigned long long)kept * 512, (uint64_t)kept * 512 >= fill_mb ? "the --fill-mb cap" : "the first failure");
    print_budget("filled");
    for (uint32_t i = 0; i < kept; i++)
        vkFreeMemory(dev, keep[i], NULL);

    vkDestroyDevice(dev, NULL);
    vkDestroyInstance(inst, NULL);
    out("%s: %d image shape(s) without memory\n", failed ? "FAIL" : "PASS", failed);
    return failed ? 1 : 0;
}
