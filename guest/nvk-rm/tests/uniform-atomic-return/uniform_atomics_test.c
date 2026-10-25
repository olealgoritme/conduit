/* SPDX-License-Identifier: Apache-2.0 */
/*
 * Small Vulkan 1.1 compute regression test for Mesa's uniform-atomics
 * rewrite (nir_opt_uniform_atomics, enabled by NAK_UNIFORM_ATOMICS=1).
 *
 * It loads a precompiled SPIR-V module produced from shader.comp by
 * glslangValidator, dispatches one 32-lane subgroup per case, and verifies
 * the returned atomicAdd values with an order-independent oracle.  An
 * affected old driver is expected externally to reject pipeline creation;
 * this harness treats every pipeline-creation error as FAIL.
 *
 * Exit status: 0 = PASS, 1 = FAIL, 77 = SKIP (prerequisites unavailable).
 */
#define _POSIX_C_SOURCE 200809L
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <inttypes.h>

#ifdef _WIN32
#define WIN32_LEAN_AND_MEAN
#define VK_NO_PROTOTYPES
#include <windows.h>
#include <vulkan/vulkan.h>

#define VK_FUNCS(X) \
    X(vkCreateInstance) X(vkDestroyInstance) X(vkEnumeratePhysicalDevices) \
    X(vkGetPhysicalDeviceProperties) X(vkGetPhysicalDeviceProperties2) \
    X(vkGetPhysicalDeviceQueueFamilyProperties) X(vkGetPhysicalDeviceMemoryProperties) \
    X(vkCreateDevice) X(vkDestroyDevice) \
    X(vkGetDeviceQueue) X(vkCreateBuffer) X(vkDestroyBuffer) \
    X(vkGetBufferMemoryRequirements) X(vkAllocateMemory) X(vkFreeMemory) \
    X(vkBindBufferMemory) X(vkMapMemory) X(vkUnmapMemory) \
    X(vkFlushMappedMemoryRanges) X(vkInvalidateMappedMemoryRanges) \
    X(vkCreateShaderModule) X(vkDestroyShaderModule) \
    X(vkCreateDescriptorSetLayout) X(vkDestroyDescriptorSetLayout) \
    X(vkCreatePipelineLayout) X(vkDestroyPipelineLayout) \
    X(vkCreateComputePipelines) X(vkDestroyPipeline) \
    X(vkCreateDescriptorPool) X(vkDestroyDescriptorPool) \
    X(vkAllocateDescriptorSets) X(vkUpdateDescriptorSets) \
    X(vkCreateCommandPool) X(vkDestroyCommandPool) \
    X(vkAllocateCommandBuffers) X(vkFreeCommandBuffers) \
    X(vkBeginCommandBuffer) X(vkEndCommandBuffer) X(vkResetCommandBuffer) \
    X(vkCmdBindPipeline) X(vkCmdBindDescriptorSets) X(vkCmdPushConstants) \
    X(vkCmdDispatch) X(vkCmdPipelineBarrier) X(vkQueueSubmit) \
    X(vkCreateFence) X(vkDestroyFence) X(vkWaitForFences) X(vkResetFences)

#define DECL(n) static PFN_##n n;
VK_FUNCS(DECL)
#undef DECL
static PFN_vkGetInstanceProcAddr p_get_proc;

static int load_global(void) {
    HMODULE m = LoadLibraryA("vulkan-1.dll");
    if (!m) return 0;
    p_get_proc = (PFN_vkGetInstanceProcAddr)GetProcAddress(m, "vkGetInstanceProcAddr");
    if (!p_get_proc) return 0;
#define LOAD_G(n) n = (PFN_##n)p_get_proc(NULL, #n);
    VK_FUNCS(LOAD_G)
#undef LOAD_G
    return vkCreateInstance != NULL;
}
static int load_instance(VkInstance inst) {
#define LOAD_I(n) n = (PFN_##n)p_get_proc(inst, #n);
    VK_FUNCS(LOAD_I)
#undef LOAD_I
    return vkEnumeratePhysicalDevices != NULL && vkDestroyInstance != NULL;
}
#else
#include <vulkan/vulkan.h>
static int load_global(void) { return 1; }
static int load_instance(VkInstance inst) { (void)inst; return 1; }
#endif

#define SENTINEL 0xDEADBEEFu
#define BUFFER_BYTES (4u + 32u * 4u)
#define FENCE_TIMEOUT_NS (10ull * 1000ull * 1000ull * 1000ull)

static int g_busy; /* a queue submission may still be in flight */

static const char *vkres(VkResult r) {
    switch ((int)r) {
    case VK_SUCCESS: return "VK_SUCCESS";
    case VK_TIMEOUT: return "VK_TIMEOUT";
    case VK_NOT_READY: return "VK_NOT_READY";
    case VK_ERROR_OUT_OF_HOST_MEMORY: return "VK_ERROR_OUT_OF_HOST_MEMORY";
    case VK_ERROR_OUT_OF_DEVICE_MEMORY: return "VK_ERROR_OUT_OF_DEVICE_MEMORY";
    case VK_ERROR_INITIALIZATION_FAILED: return "VK_ERROR_INITIALIZATION_FAILED";
    case VK_ERROR_DEVICE_LOST: return "VK_ERROR_DEVICE_LOST";
    case VK_ERROR_MEMORY_MAP_FAILED: return "VK_ERROR_MEMORY_MAP_FAILED";
    case VK_ERROR_LAYER_NOT_PRESENT: return "VK_ERROR_LAYER_NOT_PRESENT";
    case VK_ERROR_EXTENSION_NOT_PRESENT: return "VK_ERROR_EXTENSION_NOT_PRESENT";
    case VK_ERROR_FEATURE_NOT_PRESENT: return "VK_ERROR_FEATURE_NOT_PRESENT";
    case VK_ERROR_INCOMPATIBLE_DRIVER: return "VK_ERROR_INCOMPATIBLE_DRIVER";
    case VK_ERROR_TOO_MANY_OBJECTS: return "VK_ERROR_TOO_MANY_OBJECTS";
    case VK_ERROR_FORMAT_NOT_SUPPORTED: return "VK_ERROR_FORMAT_NOT_SUPPORTED";
    case VK_ERROR_UNKNOWN: return "VK_ERROR_UNKNOWN";
    default: return "other VkResult";
    }
}
#define VKOK(expr) do { VkResult r_ = (expr); \
    if (r_ != VK_SUCCESS) { \
        printf("FAIL: %s -> %d (%s)\n", #expr, (int)r_, vkres(r_)); \
        all_ok = 0; goto cleanup; \
    } } while (0)
#define VKRES(expr) do { VkResult r_ = (expr); \
    printf("%s -> %d (%s)\n", #expr, (int)r_, vkres(r_)); \
    if (r_ != VK_SUCCESS) goto cleanup; } while (0)

static int read_file(const char *path, unsigned char **data, size_t *size) {
    FILE *f = fopen(path, "rb");
    if (!f) return 0;
    if (fseek(f, 0, SEEK_END) != 0) { fclose(f); return 0; }
    long n = ftell(f);
    if (n <= 0 || fseek(f, 0, SEEK_SET) != 0) { fclose(f); return 0; }
    *data = (unsigned char *)malloc((size_t)n);
    if (!*data || fread(*data, 1, (size_t)n, f) != (size_t)n) {
        free(*data); *data = NULL; fclose(f); return 0;
    }
    fclose(f); *size = (size_t)n;
    return 1;
}

struct interval { uint64_t start, end; uint32_t lane; };

static int check_case(uint32_t mask, const uint32_t *m, uint32_t *sum_out) {
    struct interval iv[32];
    uint64_t sum = 0, expected = 0;
    int n = 0;

    for (uint32_t lane = 0; lane < 32; lane++) {
        int active = (int)((mask >> lane) & 1u);
        uint32_t old = m[1 + lane];
        if (!active) {
            if (old != SENTINEL) {
                printf("inactive lane %u = 0x%08x, expected sentinel\n", lane, old);
                return 0;
            }
            continue;
        }
        uint32_t delta = (lane % 7u) + 1u;
        iv[n].lane = lane;
        iv[n].start = old;
        iv[n].end = (uint64_t)old + delta;
        sum += delta;
        n++;
    }
    if (n == 0) {
        /* A zero mask is a valid case: no atomics, counter stays zero. */
        if (m[0] != 0) {
            printf("zero mask final counter %u, expected 0\n", m[0]);
            return 0;
        }
        *sum_out = 0;
        return 1;
    }

    /* Sort by returned-old value; no assumption about lane atomic order. */
    for (int i = 1; i < n; i++) {
        struct interval key = iv[i];
        int j = i - 1;
        while (j >= 0 && iv[j].start > key.start) { iv[j + 1] = iv[j]; j--; }
        iv[j + 1] = key;
    }
    for (int i = 0; i < n; i++) {
        if (iv[i].start != expected) {
            printf("interval lane %u starts at %" PRIu64 ", expected %" PRIu64 "\n",
                   iv[i].lane, iv[i].start, expected);
            return 0;
        }
        expected = iv[i].end;
    }
    if (expected != sum) {
        printf("intervals end at %" PRIu64 ", sum(delta)=%" PRIu64 "\n", expected, sum);
        return 0;
    }
    if (m[0] != (uint32_t)sum) {
        printf("final counter %u, sum(delta)=%" PRIu64 "\n", m[0], sum);
        return 0;
    }
    *sum_out = (uint32_t)sum;
    return 1;
}

int main(int argc, char **argv) {
    const char *spv_path = "uniform_atomics.spv";
    int allow_reference = 0;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--reference")) allow_reference = 1;
        else if (argv[i][0] != '-') spv_path = argv[i];
        else { printf("usage: %s [--reference] [shader.spv]\n", argv[0]); return 1; }
    }
#ifndef _WIN32
    /* Linux may use a normal NVIDIA reference driver only when asked. */
#else
    if (allow_reference) {
        printf("--reference is only honored on Linux; Windows requires NVK\n");
        allow_reference = 0;
    }
#endif

    int rc = 1;
    VkInstance inst = VK_NULL_HANDLE;
    VkDevice dev = VK_NULL_HANDLE;
    VkBuffer buf = VK_NULL_HANDLE;
    VkDeviceMemory mem = VK_NULL_HANDLE;
    void *mapped = NULL;
    VkShaderModule sm = VK_NULL_HANDLE;
    VkDescriptorSetLayout dsl = VK_NULL_HANDLE;
    VkPipelineLayout pl = VK_NULL_HANDLE;
    VkPipeline pipe = VK_NULL_HANDLE;
    VkDescriptorPool dpool = VK_NULL_HANDLE;
    VkDescriptorSet dset = VK_NULL_HANDLE;
    VkCommandPool cpool = VK_NULL_HANDLE;
    VkCommandBuffer cmd = VK_NULL_HANDLE;
    VkFence fence = VK_NULL_HANDLE;
    unsigned char *spv = NULL;
    size_t spv_size = 0;

    if (!load_global()) {
        printf("SKIP: Vulkan loader unavailable\n");
        return 77;
    }
    VkApplicationInfo ai = { VK_STRUCTURE_TYPE_APPLICATION_INFO, NULL,
        "uniform-atomics-regression", 1, NULL, 0, VK_API_VERSION_1_1 };
    VkInstanceCreateInfo ici = { VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, NULL, 0, &ai, 0, NULL, 0, NULL };
    VkResult r = vkCreateInstance(&ici, NULL, &inst);
    printf("vkCreateInstance -> %d (%s)\n", (int)r, vkres(r));
    if (r != VK_SUCCESS) {
        if (r == VK_ERROR_INCOMPATIBLE_DRIVER || r == VK_ERROR_FEATURE_NOT_PRESENT ||
            r == VK_ERROR_EXTENSION_NOT_PRESENT) {
            printf("SKIP: Vulkan 1.1 instance prerequisites unavailable\n");
            return 77;
        }
        printf("FAIL: vkCreateInstance failed\n");
        return 1;
    }
    if (!load_instance(inst)) { printf("SKIP: Vulkan entry points unavailable\n"); rc = 77; goto cleanup; }

    uint32_t npd = 0;
    r = vkEnumeratePhysicalDevices(inst, &npd, NULL);
    if (r != VK_SUCCESS) {
        printf("FAIL: vkEnumeratePhysicalDevices -> %d (%s)\n", (int)r, vkres(r));
        goto cleanup;
    }
    if (npd == 0) {
        printf("SKIP: no physical devices\n");
        rc = 77; goto cleanup;
    }
    VkPhysicalDevice *pds = (VkPhysicalDevice *)calloc(npd, sizeof(*pds));
    if (!pds) { printf("FAIL: out of host memory\n"); goto cleanup; }
    r = vkEnumeratePhysicalDevices(inst, &npd, pds);
    if (r != VK_SUCCESS) {
        printf("FAIL: vkEnumeratePhysicalDevices -> %d (%s)\n", (int)r, vkres(r));
        free(pds); goto cleanup;
    }

    VkPhysicalDevice pd = VK_NULL_HANDLE;
    VkPhysicalDeviceProperties pp;
    int is_nvk = 0, is_ref = 0;
    memset(&pp, 0, sizeof(pp));
    for (uint32_t i = 0; i < npd; i++) {
        VkPhysicalDeviceProperties p;
        vkGetPhysicalDeviceProperties(pds[i], &p);
        int nvk = p.vendorID == 0x10de && strstr(p.deviceName, "NVK") != NULL;
        int cpu = p.deviceType == VK_PHYSICAL_DEVICE_TYPE_CPU;
        if (nvk && !cpu) { pd = pds[i]; pp = p; is_nvk = 1; break; }
    }
    if (!pd && allow_reference) {
        for (uint32_t i = 0; i < npd; i++) {
            VkPhysicalDeviceProperties p;
            vkGetPhysicalDeviceProperties(pds[i], &p);
            int cpu = p.deviceType == VK_PHYSICAL_DEVICE_TYPE_CPU;
            if (!cpu && p.vendorID == 0x10de) { pd = pds[i]; pp = p; is_ref = 1; break; }
        }
    }
    free(pds);
    if (!pd) {
        printf("SKIP: no NVK device%s\n", allow_reference ? " and no NVIDIA reference device" : "");
        rc = 77; goto cleanup;
    }
    printf("selected physical device: %s\n", pp.deviceName);
    printf("  vendor 0x%04x device 0x%04x type %d api %u.%u driver 0x%08x (%s)\n",
           pp.vendorID, pp.deviceID, (int)pp.deviceType,
           VK_API_VERSION_MAJOR(pp.apiVersion), VK_API_VERSION_MINOR(pp.apiVersion),
           pp.driverVersion, is_nvk ? "NVK" : is_ref ? "NVIDIA reference" : "other");

#ifdef _WIN32
    if (!vkGetPhysicalDeviceProperties2) {
        printf("SKIP: vkGetPhysicalDeviceProperties2 unavailable\n");
        rc = 77; goto cleanup;
    }
#endif
    VkPhysicalDeviceSubgroupProperties subgroup;
    memset(&subgroup, 0, sizeof(subgroup));
    subgroup.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_SUBGROUP_PROPERTIES;
    VkPhysicalDeviceProperties2 props2;
    memset(&props2, 0, sizeof(props2));
    props2.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2;
    props2.pNext = &subgroup;
    vkGetPhysicalDeviceProperties2(pd, &props2);
    printf("  subgroup size %u, supported stages 0x%08x\n",
           subgroup.subgroupSize, (unsigned)subgroup.supportedStages);
    if (subgroup.subgroupSize != 32 ||
        !(subgroup.supportedStages & VK_SHADER_STAGE_COMPUTE_BIT)) {
        printf("SKIP: requires subgroupSize 32 and compute-stage subgroup support\n");
        rc = 77; goto cleanup;
    }

    uint32_t nqf = 0, qf = UINT32_MAX;
    vkGetPhysicalDeviceQueueFamilyProperties(pd, &nqf, NULL);
    if (nqf == 0) { printf("SKIP: no queue families\n"); rc = 77; goto cleanup; }
    VkQueueFamilyProperties *qfs = (VkQueueFamilyProperties *)calloc(nqf, sizeof(*qfs));
    if (!qfs) { printf("FAIL: out of host memory\n"); goto cleanup; }
    vkGetPhysicalDeviceQueueFamilyProperties(pd, &nqf, qfs);
    for (uint32_t i = 0; i < nqf; i++) {
        if ((qfs[i].queueFlags & VK_QUEUE_COMPUTE_BIT) && qfs[i].queueCount > 0) { qf = i; break; }
    }
    free(qfs);
    if (qf == UINT32_MAX) { printf("SKIP: no compute queue family\n"); rc = 77; goto cleanup; }
    printf("compute queue family: %u\n", qf);

    VkPhysicalDeviceFeatures feats;
    memset(&feats, 0, sizeof(feats)); /* no optional features or extensions */
    float prio = 1.0f;
    VkDeviceQueueCreateInfo qci = { VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, NULL, 0, qf, 1, &prio };
    VkDeviceCreateInfo dci = { VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, NULL, 0, 1, &qci, 0, NULL, 0, NULL, &feats };
    r = vkCreateDevice(pd, &dci, NULL, &dev);
    printf("vkCreateDevice -> %d (%s)\n", (int)r, vkres(r));
    if (r != VK_SUCCESS) { printf("FAIL: cannot create compute device\n"); goto cleanup; }

    if (!read_file(spv_path, &spv, &spv_size)) {
        printf("FAIL: cannot read SPIR-V '%s' (build shader.comp first)\n", spv_path);
        goto cleanup;
    }
    if (spv_size % 4 != 0) { printf("FAIL: SPIR-V size %zu is not word aligned\n", spv_size); goto cleanup; }
    VkShaderModuleCreateInfo smci = { VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO, NULL, 0, spv_size, (const uint32_t *)spv };
    VKRES(vkCreateShaderModule(dev, &smci, NULL, &sm));

    VkDescriptorSetLayoutBinding binding = { 0, VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 1, VK_SHADER_STAGE_COMPUTE_BIT, NULL };
    VkDescriptorSetLayoutCreateInfo dlci = { VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO, NULL, 0, 1, &binding };
    VKRES(vkCreateDescriptorSetLayout(dev, &dlci, NULL, &dsl));
    VkPushConstantRange pcr = { VK_SHADER_STAGE_COMPUTE_BIT, 0, sizeof(uint32_t) };
    VkPipelineLayoutCreateInfo plci = { VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO, NULL, 0, 1, &dsl, 1, &pcr };
    VKRES(vkCreatePipelineLayout(dev, &plci, NULL, &pl));

    VkPipelineShaderStageCreateInfo stage = { VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, NULL, 0,
        VK_SHADER_STAGE_COMPUTE_BIT, sm, "main", NULL };
    VkComputePipelineCreateInfo cpci = { VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO, NULL, 0,
        stage, pl, VK_NULL_HANDLE, 0 };
    r = vkCreateComputePipelines(dev, VK_NULL_HANDLE, 1, &cpci, NULL, &pipe);
    printf("vkCreateComputePipelines -> %d (%s)\n", (int)r, vkres(r));
    if (r != VK_SUCCESS) {
        printf("FAIL: pipeline creation rejected the shader (old NAK missing nir_last_invocation?)\n");
        goto cleanup;
    }

    VkBufferCreateInfo bci = { VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, NULL, 0,
        BUFFER_BYTES, VK_BUFFER_USAGE_STORAGE_BUFFER_BIT, VK_SHARING_MODE_EXCLUSIVE, 0, NULL };
    VKRES(vkCreateBuffer(dev, &bci, NULL, &buf));
    VkMemoryRequirements mr;
    vkGetBufferMemoryRequirements(dev, buf, &mr);
    VkPhysicalDeviceMemoryProperties mp;
    vkGetPhysicalDeviceMemoryProperties(pd, &mp);
    uint32_t mt = UINT32_MAX;
    for (uint32_t i = 0; i < mp.memoryTypeCount; i++) {
        VkMemoryPropertyFlags f = mp.memoryTypes[i].propertyFlags;
        if (!(mr.memoryTypeBits & (1u << i))) continue;
        if ((f & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT) && (f & VK_MEMORY_PROPERTY_HOST_COHERENT_BIT)) { mt = i; break; }
        if ((f & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT) && mt == UINT32_MAX) mt = i;
    }
    if (mt == UINT32_MAX) { printf("FAIL: no host-visible buffer memory\n"); goto cleanup; }
    int noncoherent = !(mp.memoryTypes[mt].propertyFlags & VK_MEMORY_PROPERTY_HOST_COHERENT_BIT);
    VkMemoryAllocateInfo mai = { VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, NULL, mr.size, mt };
    VKRES(vkAllocateMemory(dev, &mai, NULL, &mem));
    VKRES(vkBindBufferMemory(dev, buf, mem, 0));
    VKRES(vkMapMemory(dev, mem, 0, mr.size, 0, &mapped));
    printf("buffer memory type %u (%scoherent), mapped %" PRIu64 " bytes\n",
           mt, noncoherent ? "non-" : "", (uint64_t)mr.size);

    VkDescriptorPoolSize psz = { VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 1 };
    VkDescriptorPoolCreateInfo dpci = { VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO, NULL, 0, 1, 1, &psz };
    VKRES(vkCreateDescriptorPool(dev, &dpci, NULL, &dpool));
    VkDescriptorSetAllocateInfo dsai = { VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO, NULL, dpool, 1, &dsl };
    VKRES(vkAllocateDescriptorSets(dev, &dsai, &dset));
    VkDescriptorBufferInfo dbi = { buf, 0, VK_WHOLE_SIZE };
    VkWriteDescriptorSet wds = { VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, NULL, dset, 0, 0, 1,
        VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, NULL, &dbi, NULL };
    vkUpdateDescriptorSets(dev, 1, &wds, 0, NULL);

    VkCommandPoolCreateInfo cpi = { VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, NULL,
        VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT, qf };
    VKRES(vkCreateCommandPool(dev, &cpi, NULL, &cpool));
    VkCommandBufferAllocateInfo cbai = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, NULL,
        cpool, VK_COMMAND_BUFFER_LEVEL_PRIMARY, 1 };
    VKRES(vkAllocateCommandBuffers(dev, &cbai, &cmd));
    VkFenceCreateInfo fci = { VK_STRUCTURE_TYPE_FENCE_CREATE_INFO, NULL, 0 };
    VKRES(vkCreateFence(dev, &fci, NULL, &fence));

    VkQueue queue = VK_NULL_HANDLE;
    vkGetDeviceQueue(dev, qf, 0, &queue);
    if (!queue) { printf("FAIL: no compute queue\n"); goto cleanup; }

    struct testcase { uint32_t mask; const char *desc; } cases[] = {
        { 0xffffffffu, "all 32 lanes" },
        { 0x80000001u, "lanes 0 and 31 (last active = 31)" },
        { 0x7fffffffu, "lanes 0..30 (lane 31 inactive, last active = 30)" },
        { 0x80000000u, "lane 31 only" },
        { 0x40000000u, "lane 30 only (last active excludes 31)" },
        { 0x00000005u, "lanes 0 and 2" },
        { 0x00010010u, "lanes 4 and 16" },
        { 0x55555555u, "even lanes 0..30" },
        { 0x00000000u, "zero mask (no active lanes)" },
    };
    uint32_t *u = (uint32_t *)mapped;
    int all_ok = 1;

    for (size_t c = 0; c < sizeof(cases) / sizeof(cases[0]); c++) {
        u[0] = 0;
        for (uint32_t lane = 0; lane < 32; lane++) u[1 + lane] = SENTINEL;
        if (noncoherent) {
            VkMappedMemoryRange mrng = { VK_STRUCTURE_TYPE_MAPPED_MEMORY_RANGE, NULL, mem, 0, VK_WHOLE_SIZE };
            VKOK(vkFlushMappedMemoryRanges(dev, 1, &mrng));
        }

        VKOK(vkResetCommandBuffer(cmd, 0));
        VkCommandBufferBeginInfo bi = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO, NULL, 0, NULL };
        VKOK(vkBeginCommandBuffer(cmd, &bi));
        VkMemoryBarrier pre = { VK_STRUCTURE_TYPE_MEMORY_BARRIER, NULL,
            VK_ACCESS_HOST_WRITE_BIT, VK_ACCESS_SHADER_READ_BIT | VK_ACCESS_SHADER_WRITE_BIT };
        vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_HOST_BIT, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
                             0, 1, &pre, 0, NULL, 0, NULL);
        vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_COMPUTE, pipe);
        vkCmdBindDescriptorSets(cmd, VK_PIPELINE_BIND_POINT_COMPUTE, pl, 0, 1, &dset, 0, NULL);
        vkCmdPushConstants(cmd, pl, VK_SHADER_STAGE_COMPUTE_BIT, 0, sizeof(uint32_t), &cases[c].mask);
        vkCmdDispatch(cmd, 1, 1, 1);
        VkMemoryBarrier post = { VK_STRUCTURE_TYPE_MEMORY_BARRIER, NULL,
            VK_ACCESS_SHADER_WRITE_BIT, VK_ACCESS_HOST_READ_BIT };
        vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_PIPELINE_STAGE_HOST_BIT,
                             0, 1, &post, 0, NULL, 0, NULL);
        VKOK(vkEndCommandBuffer(cmd));

        VKOK(vkResetFences(dev, 1, &fence));
        VkSubmitInfo si = { VK_STRUCTURE_TYPE_SUBMIT_INFO, NULL, 0, NULL, NULL, 1, &cmd, 0, NULL };
        VkResult sr = vkQueueSubmit(queue, 1, &si, fence);
        if (sr != VK_SUCCESS) {
            printf("FAIL: vkQueueSubmit -> %d (%s)\n", (int)sr, vkres(sr));
            all_ok = 0; g_busy = 1; break;
        }
        g_busy = 1;
        sr = vkWaitForFences(dev, 1, &fence, VK_TRUE, FENCE_TIMEOUT_NS);
        if (sr == VK_TIMEOUT) {
            printf("FAIL: fence timeout after 10 seconds\n");
            all_ok = 0; break;
        }
        if (sr != VK_SUCCESS) {
            printf("FAIL: vkWaitForFences -> %d (%s)\n", (int)sr, vkres(sr));
            all_ok = 0; break;
        }
        g_busy = 0;

        if (noncoherent) {
            VkMappedMemoryRange mrng = { VK_STRUCTURE_TYPE_MAPPED_MEMORY_RANGE, NULL, mem, 0, VK_WHOLE_SIZE };
            VkResult ir = vkInvalidateMappedMemoryRanges(dev, 1, &mrng);
            if (ir != VK_SUCCESS) {
                printf("FAIL: vkInvalidateMappedMemoryRanges -> %d (%s)\n", (int)ir, vkres(ir));
                all_ok = 0; break;
            }
        }

        uint32_t sum = 0;
        printf("case %zu mask=0x%08x (%s): ", c, cases[c].mask, cases[c].desc);
        if (!check_case(cases[c].mask, u, &sum)) { printf("FAIL\n"); all_ok = 0; break; }
        printf("OK counter=%u sum(delta)=%u\n", u[0], sum);
    }

    if (all_ok) {
        printf("all %zu GPU cases passed\n", sizeof(cases) / sizeof(cases[0]));
        rc = 0;
    }

cleanup:
    free(spv);
    if (g_busy) {
        printf("cleanup: queue may still be busy; skipping Vulkan destruction\n");
    } else {
        if (dev != VK_NULL_HANDLE) {
            if (fence != VK_NULL_HANDLE) vkDestroyFence(dev, fence, NULL);
            if (cmd != VK_NULL_HANDLE) {
                vkResetCommandBuffer(cmd, 0); /* also leaves a failed recording state */
                vkFreeCommandBuffers(dev, cpool, 1, &cmd);
            }
            if (cpool != VK_NULL_HANDLE) vkDestroyCommandPool(dev, cpool, NULL);
            if (dpool != VK_NULL_HANDLE) vkDestroyDescriptorPool(dev, dpool, NULL);
            if (pipe != VK_NULL_HANDLE) vkDestroyPipeline(dev, pipe, NULL);
            if (pl != VK_NULL_HANDLE) vkDestroyPipelineLayout(dev, pl, NULL);
            if (dsl != VK_NULL_HANDLE) vkDestroyDescriptorSetLayout(dev, dsl, NULL);
            if (sm != VK_NULL_HANDLE) vkDestroyShaderModule(dev, sm, NULL);
            if (mapped) vkUnmapMemory(dev, mem);
            if (buf != VK_NULL_HANDLE) vkDestroyBuffer(dev, buf, NULL);
            if (mem != VK_NULL_HANDLE) vkFreeMemory(dev, mem, NULL);
            vkDestroyDevice(dev, NULL);
        }
        if (inst != VK_NULL_HANDLE) vkDestroyInstance(inst, NULL);
    }

    if (rc == 0) printf("PASS\n");
    else if (rc == 77) printf("SKIP 77\n");
    else printf("FAIL\n");
    return rc;
}
