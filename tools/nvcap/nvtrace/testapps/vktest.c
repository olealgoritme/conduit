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
 * Options: --async-compute  use a compute-only queue family if there is one
 * Prints step-by-step progress and a final "PASS" or "FAIL: <reason>" line.
 */
#include "common.h"
#include <stdint.h>

#define VK_NO_PROTOTYPES
#include <vulkan/vulkan_core.h>

#include "vktest_comp.h" /* vktest_comp_spv[], generated from vktest.comp */

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
    X(vkCreateInstance) X(vkEnumerateInstanceVersion)
#define VK_INSTANCE_FUNCS(X)                                                        \
    X(vkDestroyInstance) X(vkEnumeratePhysicalDevices) X(vkGetPhysicalDeviceProperties2) \
    X(vkGetPhysicalDeviceQueueFamilyProperties) X(vkGetPhysicalDeviceMemoryProperties)  \
    X(vkGetPhysicalDeviceFeatures2) X(vkCreateDevice) X(vkGetDeviceProcAddr)
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
    X(vkGetSemaphoreCounterValue)

#define DECLARE(name) static PFN_##name name;
VK_GLOBAL_FUNCS(DECLARE)
VK_INSTANCE_FUNCS(DECLARE)
VK_DEVICE_FUNCS(DECLARE)
static PFN_vkGetInstanceProcAddr vkGetInstanceProcAddr;

static const char *missing_fn;

static int load_global_funcs(void)
{
#define LOAD_G(name)                                                              \
    name = (PFN_##name)vkGetInstanceProcAddr(VK_NULL_HANDLE, #name);              \
    if (!name && strcmp(#name, "vkEnumerateInstanceVersion")) { missing_fn = #name; return 0; }
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

static int load_device_funcs(VkDevice dev)
{
#define LOAD_D(name)                                                              \
    name = (PFN_##name)vkGetDeviceProcAddr(dev, #name);                           \
    if (!name) { missing_fn = #name; return 0; }
    VK_DEVICE_FUNCS(LOAD_D)
    return 1;
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

static int create_buffer(Buffer *b, const char *name, VkBufferUsageFlags usage, int host_visible)
{
    VkBufferCreateInfo bci = {.sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO};
    VkMemoryAllocateInfo mai = {.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO};
    VkMemoryRequirements req;
    int type;

    b->name = name;
    b->map = NULL;
    bci.size = BUF_BYTES;
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
    logmsg("buffer %-8s: %llu bytes, memory type %d (flags 0x%x, heap %u)%s", name,
           (unsigned long long)req.size, type, mem_props.memoryTypes[type].propertyFlags,
           mem_props.memoryTypes[type].heapIndex, host_visible ? ", mapped" : "");
    return 0;
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
    double t0;
    char name[VK_MAX_PHYSICAL_DEVICE_NAME_SIZE + 1];

    common_init("vktest");

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

    logmsg("vkCreateInstance (apiVersion 1.2, no layers, no extensions)");
    app.pApplicationName = "nvtrace-vktest";
    app.applicationVersion = 1;
    app.pEngineName = "none";
    app.apiVersion = VK_API_VERSION_1_2;
    ici.pApplicationInfo = &app;
    VK_CHECK(vkCreateInstance(&ici, NULL, &inst));
    if (!load_instance_funcs(inst))
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

    logmsg("vkCreateDevice (1 queue, timelineSemaphore)");
    dqci.queueFamilyIndex = qfam;
    dqci.queueCount = 1;
    dqci.pQueuePriorities = &prio;
    memset(&f12, 0, sizeof(f12));
    f12.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES;
    f12.timelineSemaphore = VK_TRUE;
    dci.pNext = &f12;
    dci.queueCreateInfoCount = 1;
    dci.pQueueCreateInfos = &dqci;
    VK_CHECK(vkCreateDevice(pdev, &dci, NULL, &device));
    if (!load_device_funcs(device))
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

    logmsg("cleanup");
    VK_CHECK(vkDeviceWaitIdle(device));
    vkDestroySemaphore(device, timeline, NULL);
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
