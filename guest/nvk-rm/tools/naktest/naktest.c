// naktest: hardware validation of NAK's instruction scheduling on the GPU.
//
// Runs compute shaders whose every instruction depends on the one before
// (FMA chains, integer ALU chains, LOP3/SHF/PRMT, predicate guards from
// small ifs, 64-bit carry chains, uniform-datapath chains) and shaders with
// more independent loads in flight than there are scoreboards, texture
// instructions in the source layouts of NAK_TEX_UHANDLE, and compares
// every output word with a CPU reference.  A compiler that waits too few
// cycles between producer and consumer (NAK_LAT) or shares a scoreboard
// wrongly (NAK_BAR_SHARE) makes a consumer read a stale register, and the
// output differs.  Run it with the knobs under test set in the environment:
//
//   naktest [-r REPEATS] [-n INVOCATIONS] [-t NAME_PREFIX]
//
// Exit status 0: every test passed; 1: a mismatch; 2: no device / setup error.
#ifdef _WIN32
#define VK_NO_PROTOTYPES
#include <windows.h>
#endif
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>

#ifdef _WIN32
#define FNS(X) X(vkCreateInstance) X(vkEnumeratePhysicalDevices) X(vkGetPhysicalDeviceProperties) \
  X(vkGetPhysicalDeviceMemoryProperties) X(vkGetPhysicalDeviceQueueFamilyProperties) X(vkGetPhysicalDeviceFeatures2) \
  X(vkCreateDevice) X(vkGetDeviceQueue) X(vkCreateBuffer) X(vkGetBufferMemoryRequirements) X(vkAllocateMemory) \
  X(vkBindBufferMemory) X(vkMapMemory) X(vkCreateDescriptorSetLayout) X(vkCreatePipelineLayout) X(vkCreateShaderModule) \
  X(vkCreateComputePipelines) X(vkCreateDescriptorPool) X(vkAllocateDescriptorSets) X(vkUpdateDescriptorSets) \
  X(vkCreateCommandPool) X(vkAllocateCommandBuffers) X(vkBeginCommandBuffer) X(vkCmdBindPipeline) \
  X(vkCmdBindDescriptorSets) X(vkCmdPushConstants) X(vkCmdDispatch) X(vkEndCommandBuffer) X(vkQueueSubmit) \
  X(vkQueueWaitIdle) X(vkDestroyPipeline) X(vkDestroyShaderModule) X(vkResetCommandBuffer) X(vkCmdFillBuffer) \
  X(vkCmdPipelineBarrier) X(vkDestroyDevice) X(vkDestroyInstance) X(vkGetInstanceProcAddr) X(vkCreateImage) \
  X(vkGetImageMemoryRequirements) X(vkBindImageMemory) X(vkCreateImageView) X(vkCreateSampler) X(vkCmdCopyBufferToImage)
#define DECL(n) static PFN_##n n;
FNS(DECL)
static int load_vk(void) {
    HMODULE m = LoadLibraryA("vulkan-1.dll");
    if (!m) return 0;
    vkGetInstanceProcAddr = (PFN_vkGetInstanceProcAddr)GetProcAddress(m, "vkGetInstanceProcAddr");
    vkCreateInstance = (PFN_vkCreateInstance)vkGetInstanceProcAddr(NULL, "vkCreateInstance");
    return vkCreateInstance != NULL;
}
static void load_inst(VkInstance i) {
#define LOADI(n) if (!n) n = (PFN_##n)vkGetInstanceProcAddr(i, #n);
    FNS(LOADI)
}
#else
static int load_vk(void) { return 1; }
static void load_inst(VkInstance i) { (void)i; }
#endif

#include "gen/tests.h"

#define CHK(x) do { VkResult r_ = (x); if (r_ != VK_SUCCESS) { fprintf(stderr, "%s failed: %d\n", #x, r_); exit(2); } } while (0)

static uint32_t mem_type(VkPhysicalDeviceMemoryProperties *mp, uint32_t bits, VkMemoryPropertyFlags want) {
    for (uint32_t i = 0; i < mp->memoryTypeCount; i++)
        if ((bits & (1u << i)) && (mp->memoryTypes[i].propertyFlags & want) == want) return i;
    fprintf(stderr, "no memory type with flags 0x%x\n", want); exit(2);
}

/* The textures of the "tex" tests: a 64x64 2D array with 4 layers, @mips
 * levels, texel (mip << 24 | layer << 16 | y << 8 | x) for R32_UINT, depth
 * ((x * 7 + y * 13 + layer * 29) & 255) / 256 for D32_SFLOAT; uploaded and
 * left in SHADER_READ_ONLY_OPTIMAL
 */
static VkImageView make_tex(VkDevice dev, VkPhysicalDeviceMemoryProperties *mp, VkQueue q, VkCommandBuffer cb,
                            VkFormat fmt, uint32_t mips) {
    const int depth = fmt == VK_FORMAT_D32_SFLOAT;
    VkImageCreateInfo ic = { VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO, NULL, 0, VK_IMAGE_TYPE_2D, fmt, { 64, 64, 1 }, mips, 4,
        VK_SAMPLE_COUNT_1_BIT, VK_IMAGE_TILING_OPTIMAL, VK_IMAGE_USAGE_SAMPLED_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT,
        VK_SHARING_MODE_EXCLUSIVE, 0, NULL, VK_IMAGE_LAYOUT_UNDEFINED };
    VkImage img; CHK(vkCreateImage(dev, &ic, NULL, &img));
    VkMemoryRequirements mr; vkGetImageMemoryRequirements(dev, img, &mr);
    VkMemoryAllocateInfo mai = { VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, NULL, mr.size,
                                 mem_type(mp, mr.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT) };
    VkDeviceMemory m; CHK(vkAllocateMemory(dev, &mai, NULL, &m)); CHK(vkBindImageMemory(dev, img, m, 0));

    VkDeviceSize size = 0;
    for (uint32_t l = 0; l < mips; l++) size += (VkDeviceSize)(64 >> l) * (64 >> l) * 4 * 4;
    VkBufferCreateInfo bci = { VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, NULL, 0, size, VK_BUFFER_USAGE_TRANSFER_SRC_BIT,
                               VK_SHARING_MODE_EXCLUSIVE };
    VkBuffer sb; CHK(vkCreateBuffer(dev, &bci, NULL, &sb));
    vkGetBufferMemoryRequirements(dev, sb, &mr);
    mai = (VkMemoryAllocateInfo){ VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, NULL, mr.size,
        mem_type(mp, mr.memoryTypeBits, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT) };
    VkDeviceMemory sm; CHK(vkAllocateMemory(dev, &mai, NULL, &sm)); CHK(vkBindBufferMemory(dev, sb, sm, 0));
    uint32_t *p; CHK(vkMapMemory(dev, sm, 0, VK_WHOLE_SIZE, 0, (void **)&p));
    VkBufferImageCopy rg[8]; VkDeviceSize off = 0;
    const VkImageAspectFlags aspect = depth ? VK_IMAGE_ASPECT_DEPTH_BIT : VK_IMAGE_ASPECT_COLOR_BIT;
    for (uint32_t lv = 0; lv < mips; lv++) {
        const uint32_t w = 64 >> lv;
        uint32_t *t = p + off / 4;
        for (uint32_t l = 0; l < 4; l++)
            for (uint32_t y = 0; y < w; y++)
                for (uint32_t x = 0; x < w; x++) {
                    uint32_t *e = &t[(l * w + y) * w + x];
                    if (depth) { float d = (float)((x * 7u + y * 13u + l * 29u) & 255u) / 256.0f; memcpy(e, &d, 4); }
                    else *e = lv << 24 | l << 16 | y << 8 | x;
                }
        rg[lv] = (VkBufferImageCopy){ off, 0, 0, { aspect, lv, 0, 4 }, { 0, 0, 0 }, { w, w, 1 } };
        off += (VkDeviceSize)w * w * 4 * 4;
    }
    VkCommandBufferBeginInfo bi = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO, NULL, VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT };
    CHK(vkBeginCommandBuffer(cb, &bi));
    VkImageMemoryBarrier ib = { VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER, NULL, 0, VK_ACCESS_TRANSFER_WRITE_BIT,
        VK_IMAGE_LAYOUT_UNDEFINED, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, VK_QUEUE_FAMILY_IGNORED, VK_QUEUE_FAMILY_IGNORED,
        img, { aspect, 0, mips, 0, 4 } };
    vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL, 0, NULL, 1, &ib);
    vkCmdCopyBufferToImage(cb, sb, img, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, mips, rg);
    ib.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT; ib.dstAccessMask = VK_ACCESS_SHADER_READ_BIT;
    ib.oldLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL; ib.newLayout = VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL;
    vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, 0, 0, NULL, 0, NULL, 1, &ib);
    CHK(vkEndCommandBuffer(cb));
    VkSubmitInfo si = { VK_STRUCTURE_TYPE_SUBMIT_INFO, NULL, 0, NULL, NULL, 1, &cb };
    CHK(vkQueueSubmit(q, 1, &si, VK_NULL_HANDLE)); CHK(vkQueueWaitIdle(q));
    CHK(vkResetCommandBuffer(cb, 0));
    VkImageViewCreateInfo vc = { VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO, NULL, 0, img, VK_IMAGE_VIEW_TYPE_2D_ARRAY, fmt,
                                 { 0, 0, 0, 0 }, { aspect, 0, mips, 0, 4 } };
    VkImageView v; CHK(vkCreateImageView(dev, &vc, NULL, &v));
    return v;
}

int main(int argc, char **argv) {
    int repeats = 20; uint32_t n = 65536; const char *only = NULL;
    for (int a = 1; a < argc; a++) {
        if (!strcmp(argv[a], "-r") && a + 1 < argc) repeats = atoi(argv[++a]);
        else if (!strcmp(argv[a], "-n") && a + 1 < argc) n = (uint32_t)atoi(argv[++a]) & ~63u;
        else if (!strcmp(argv[a], "-t") && a + 1 < argc) only = argv[++a];
        else { fprintf(stderr, "usage: naktest [-r REPEATS] [-n INVOCATIONS] [-t NAME_PREFIX]\n"); return 2; }
    }
    if (!load_vk()) { fprintf(stderr, "no Vulkan loader\n"); return 2; }
    const char *knobs[] = { "NAK_TEX_UHANDLE", "NAK_LAT", "NAK_BAR_SHARE", "NAK_IF_CONVERT", "NAK_UADDR_PIN", "NAK_PLOP_FOLD", "NAK_IMM_CSE",
                            "NAK_PHI_AVOID", "NAK_WEAK_MEM", "NAK_VEC_CSE", "NAK_UNROLL_MAX", "NAK_BSSY_RECONVERGENT", "NAK_NIR_FLATTEN", "NAK_OCC_TARGET", "MESA_SHADER_CACHE_DISABLE" };
    printf("naktest: %u invocations, %d repeats; environment:", n, repeats);
    for (size_t k = 0; k < sizeof(knobs) / sizeof(knobs[0]); k++) { const char *v = getenv(knobs[k]); if (v) printf(" %s=%s", knobs[k], v); }
    printf("\n");

    VkApplicationInfo ai = { VK_STRUCTURE_TYPE_APPLICATION_INFO, NULL, "naktest", 1, NULL, 0, VK_API_VERSION_1_2 };
    VkInstanceCreateInfo ici = { VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, NULL, 0, &ai };
    VkInstance inst; CHK(vkCreateInstance(&ici, NULL, &inst)); load_inst(inst);
    uint32_t np = 8; VkPhysicalDevice pds[8]; CHK(vkEnumeratePhysicalDevices(inst, &np, pds));
    VkPhysicalDevice pd = VK_NULL_HANDLE; VkPhysicalDeviceProperties pp;
    for (uint32_t i = 0; i < np; i++) {
        vkGetPhysicalDeviceProperties(pds[i], &pp);
        if (strstr(pp.deviceName, "NVK") || pp.vendorID == 0x10de) { pd = pds[i]; break; }
    }
    if (!pd) { fprintf(stderr, "no NVIDIA/NVK device\n"); return 2; }
    printf("device: %s, driver version 0x%x\n", pp.deviceName, pp.driverVersion);

    VkPhysicalDeviceFeatures2 f2 = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2 };
    vkGetPhysicalDeviceFeatures2(pd, &f2);
    VkPhysicalDeviceFeatures en = { 0 }; en.shaderInt64 = f2.features.shaderInt64;
    uint32_t nq = 16; VkQueueFamilyProperties qf[16]; vkGetPhysicalDeviceQueueFamilyProperties(pd, &nq, qf);
    uint32_t qfi = 0; while (qfi < nq && !(qf[qfi].queueFlags & VK_QUEUE_COMPUTE_BIT)) qfi++;
    float prio = 1; VkDeviceQueueCreateInfo qci = { VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, NULL, 0, qfi, 1, &prio };
    VkDeviceCreateInfo dci = { VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, NULL, 0, 1, &qci, 0, NULL, 0, NULL, &en };
    VkDevice dev; CHK(vkCreateDevice(pd, &dci, NULL, &dev));
    VkQueue q; vkGetDeviceQueue(dev, qfi, 0, &q);

    VkPhysicalDeviceMemoryProperties mp; vkGetPhysicalDeviceMemoryProperties(pd, &mp);
    VkBuffer buf[2]; VkDeviceMemory mem[2]; uint32_t *ptr[2];
    for (int b = 0; b < 2; b++) {
        VkBufferCreateInfo bci = { VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, NULL, 0, (VkDeviceSize)n * 4,
            VK_BUFFER_USAGE_STORAGE_BUFFER_BIT | VK_BUFFER_USAGE_TRANSFER_DST_BIT, VK_SHARING_MODE_EXCLUSIVE };
        CHK(vkCreateBuffer(dev, &bci, NULL, &buf[b]));
        VkMemoryRequirements mr; vkGetBufferMemoryRequirements(dev, buf[b], &mr);
        VkMemoryAllocateInfo mai = { VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, NULL, mr.size,
            mem_type(&mp, mr.memoryTypeBits, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT) };
        CHK(vkAllocateMemory(dev, &mai, NULL, &mem[b])); CHK(vkBindBufferMemory(dev, buf[b], mem[b], 0));
        CHK(vkMapMemory(dev, mem[b], 0, VK_WHOLE_SIZE, 0, (void **)&ptr[b]));
    }
    VkDescriptorSetLayoutBinding lb[4] = { { 0, VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 1, VK_SHADER_STAGE_COMPUTE_BIT, NULL },
                                           { 1, VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 1, VK_SHADER_STAGE_COMPUTE_BIT, NULL },
                                           { 2, VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, 1, VK_SHADER_STAGE_COMPUTE_BIT, NULL },
                                           { 3, VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, 1, VK_SHADER_STAGE_COMPUTE_BIT, NULL } };
    VkDescriptorSetLayoutCreateInfo lci = { VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO, NULL, 0, 4, lb };
    VkDescriptorSetLayout dsl; CHK(vkCreateDescriptorSetLayout(dev, &lci, NULL, &dsl));
    VkPushConstantRange pcr = { VK_SHADER_STAGE_COMPUTE_BIT, 0, 32 };
    VkPipelineLayoutCreateInfo plci = { VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO, NULL, 0, 1, &dsl, 1, &pcr };
    VkPipelineLayout pl; CHK(vkCreatePipelineLayout(dev, &plci, NULL, &pl));
    VkDescriptorPoolSize ps[2] = { { VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 2 }, { VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, 2 } };
    VkDescriptorPoolCreateInfo dpci = { VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO, NULL, 0, 1, 2, ps };
    VkDescriptorPool dp; CHK(vkCreateDescriptorPool(dev, &dpci, NULL, &dp));
    VkDescriptorSetAllocateInfo dsai = { VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO, NULL, dp, 1, &dsl };
    VkDescriptorSet ds; CHK(vkAllocateDescriptorSets(dev, &dsai, &ds));
    VkCommandPoolCreateInfo cpci = { VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, NULL, VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT, qfi };
    VkCommandPool cp; CHK(vkCreateCommandPool(dev, &cpci, NULL, &cp));
    VkCommandBufferAllocateInfo cbai = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, NULL, cp, VK_COMMAND_BUFFER_LEVEL_PRIMARY, 1 };
    VkCommandBuffer cb; CHK(vkAllocateCommandBuffers(dev, &cbai, &cb));
    VkImageView utv = make_tex(dev, &mp, q, cb, VK_FORMAT_R32_UINT, 3);
    VkImageView stv = make_tex(dev, &mp, q, cb, VK_FORMAT_D32_SFLOAT, 1);
    VkSamplerCreateInfo sci = { VK_STRUCTURE_TYPE_SAMPLER_CREATE_INFO, NULL, 0, VK_FILTER_NEAREST, VK_FILTER_NEAREST,
        VK_SAMPLER_MIPMAP_MODE_NEAREST, VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE, VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
        VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE, 0, VK_FALSE, 1, VK_FALSE, VK_COMPARE_OP_ALWAYS, 0, 16,
        VK_BORDER_COLOR_INT_TRANSPARENT_BLACK, VK_FALSE };
    VkSampler us; CHK(vkCreateSampler(dev, &sci, NULL, &us));
    sci.compareEnable = VK_TRUE; sci.compareOp = VK_COMPARE_OP_LESS_OR_EQUAL;
    sci.borderColor = VK_BORDER_COLOR_FLOAT_TRANSPARENT_BLACK;
    VkSampler ss; CHK(vkCreateSampler(dev, &sci, NULL, &ss));
    VkDescriptorBufferInfo dbi[2] = { { buf[0], 0, VK_WHOLE_SIZE }, { buf[1], 0, VK_WHOLE_SIZE } };
    VkDescriptorImageInfo dii[2] = { { us, utv, VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL },
                                     { ss, stv, VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL } };
    VkWriteDescriptorSet w[4];
    for (int b = 0; b < 2; b++) w[b] = (VkWriteDescriptorSet){ VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, NULL, ds, (uint32_t)b, 0, 1,
        VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, NULL, &dbi[b], NULL };
    for (int b = 0; b < 2; b++) w[2 + b] = (VkWriteDescriptorSet){ VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, NULL, ds,
        (uint32_t)(2 + b), 0, 1, VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, &dii[b], NULL, NULL };
    vkUpdateDescriptorSets(dev, 4, w, 0, NULL);

    uint32_t *expect = malloc((size_t)n * 4);
    int failed = 0, ran = 0;
    for (size_t t = 0; t < sizeof(tests) / sizeof(tests[0]); t++) {
        const struct test *T = &tests[t];
        if (only && strncmp(T->name, only, strlen(only))) continue;
        if (!strcmp(T->kind, "carry") && !en.shaderInt64) { printf("%-10s skipped (no shaderInt64)\n", T->name); continue; }
        VkShaderModuleCreateInfo smci = { VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO, NULL, 0, T->spv_size, T->spv };
        VkShaderModule sm; CHK(vkCreateShaderModule(dev, &smci, NULL, &sm));
        VkComputePipelineCreateInfo cpi = { VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO, NULL, 0,
            { VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, NULL, 0, VK_SHADER_STAGE_COMPUTE_BIT, sm, "main", NULL }, pl, VK_NULL_HANDLE, 0 };
        VkPipeline pipe; CHK(vkCreateComputePipelines(dev, VK_NULL_HANDLE, 1, &cpi, NULL, &pipe));
        long bad = 0; uint32_t first_i = 0, first_got = 0, first_want = 0;
        for (int r = 0; r < repeats; r++) {
            uint32_t s = 0x9e3779b9u * (uint32_t)(r + 1) ^ (uint32_t)t * 0x85ebca6bu;
            for (uint32_t i = 0; i < n; i++) { s ^= s << 13; s ^= s >> 17; s ^= s << 5; ptr[0][i] = s; ptr[1][i] = 0xdeadbeefu; }
            uint32_t pc[8];
            for (int k = 0; k < 8; k++) { s ^= s << 13; s ^= s >> 17; s ^= s << 5; pc[k] = s; }
            pc[1] = n - 1;            /* index mask of the load tests (n is a power of two) */
            if (n & (n - 1)) pc[1] = 4095;
            VkCommandBufferBeginInfo bi = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO, NULL, VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT };
            CHK(vkBeginCommandBuffer(cb, &bi));
            vkCmdBindPipeline(cb, VK_PIPELINE_BIND_POINT_COMPUTE, pipe);
            vkCmdBindDescriptorSets(cb, VK_PIPELINE_BIND_POINT_COMPUTE, pl, 0, 1, &ds, 0, NULL);
            vkCmdPushConstants(cb, pl, VK_SHADER_STAGE_COMPUTE_BIT, 0, 32, pc);
            vkCmdDispatch(cb, n / 64, 1, 1);
            CHK(vkEndCommandBuffer(cb));
            VkSubmitInfo si = { VK_STRUCTURE_TYPE_SUBMIT_INFO, NULL, 0, NULL, NULL, 1, &cb };
            CHK(vkQueueSubmit(q, 1, &si, VK_NULL_HANDLE)); CHK(vkQueueWaitIdle(q));
            for (uint32_t i = 0; i < n; i++) expect[i] = T->ref(ptr[0], i, pc);
            for (uint32_t i = 0; i < n; i++) if (ptr[1][i] != expect[i]) {
                if (!bad) { first_i = i; first_got = ptr[1][i]; first_want = expect[i]; }
                bad++;
            }
            CHK(vkResetCommandBuffer(cb, 0));
        }
        ran++;
        if (bad) { failed++; printf("%-10s %-8s FAIL: %ld wrong words, first at %u: got 0x%08x want 0x%08x\n", T->name, T->kind, bad, first_i, first_got, first_want); }
        else printf("%-10s %-8s ok\n", T->name, T->kind);
        fflush(stdout);
        vkDestroyPipeline(dev, pipe, NULL); vkDestroyShaderModule(dev, sm, NULL);
    }
    printf("naktest: %d of %d tests failed\n", failed, ran);
    vkDestroyDevice(dev, NULL); vkDestroyInstance(inst, NULL);
    return failed ? 1 : 0;
}
