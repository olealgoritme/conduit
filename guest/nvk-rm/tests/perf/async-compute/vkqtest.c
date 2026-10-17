/* SPDX-License-Identifier: MIT */
/*
 * vkqtest: does the Vulkan driver run a compute-only queue concurrently with
 * the graphics queue?  One device, queue family 0 (graphics) and the
 * compute-only family.  Each queue gets a few SMs of busy-wait work (a
 * compute dispatch of a few workgroups, a draw of an 8x8-pixel quad whose
 * fragment shader spins), bracketed by vkCmdWriteTimestamp.  Cases:
 *
 *   gfx    graphics alone        comp   compute alone
 *   both   graphics + compute submitted together (both gated on one timeline
 *          semaphore the CPU signals after both submits)
 *   cc     compute queue 0 + compute queue 1 together (with --cq >= 2)
 *   gg     graphics queue 0 + graphics queue 1 together (with --gq >= 2)
 *   g1c    graphics queue 1 + compute queue 0 together (with --gq >= 2)
 *   both1  graphics queue 0 + compute queue 1 together (with --cq >= 2)
 *
 * All timestamps come from one device timestamp domain (vkCmdWriteTimestamp on
 * both queues of one VkDevice), converted with timestampPeriod.
 *
 *   makespan = max(end) - min(start) over the two queues, medians over --reps
 *   overlap  = 1 - (makespan - longest solo) / (shorter solo)
 *   CONCURRENT >= 75%, SERIAL <= 25%, PARTIAL in between (as async_compute_probe)
 *
 * If the process runs under librmtrace.so, markers go into the RM trace
 * (rmtrace_mark, looked up with dlsym).
 *
 * Usage: vkqtest [--cq n] [--gq n] [--spin-us us] [--reps n] [--wgs n] [--px n]
 *                [--cases list] [--trace-only]
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <vulkan/vulkan.h>

#include "shaders.h"

#define CHECK(x)                                                                                                     \
    do {                                                                                                             \
        VkResult r_ = (x);                                                                                           \
        if (r_ != VK_SUCCESS) {                                                                                      \
            fprintf(stderr, "%s:%d: %s -> %d\n", __FILE__, __LINE__, #x, r_);                                       \
            exit(2);                                                                                                 \
        }                                                                                                            \
    } while (0)

static void (*mark_fn)(const char *);
static void mark(const char *s)
{
    if (mark_fn)
        mark_fn(s);
    printf("-- %s\n", s);
    fflush(stdout);
}

#define MAXQ 16
static struct {
    VkInstance inst;
    VkPhysicalDevice pd;
    VkDevice dev;
    uint32_t gfam, cfam, ngq, ncq;
    VkQueue gq[MAXQ], cq[MAXQ];
    VkCommandPool gpool, cpool;
    float tsperiod;
    VkQueryPool qp;
    VkSemaphore gate;
    uint64_t gate_val;
    /* compute */
    VkBuffer buf;
    VkDeviceMemory bufmem;
    VkDescriptorSetLayout dsl;
    VkDescriptorPool dpool;
    VkDescriptorSet ds;
    VkPipelineLayout cpl;
    VkPipeline cpipe;
    /* graphics */
    VkImage img;
    VkDeviceMemory imgmem;
    VkImageView view;
    VkRenderPass rp;
    VkFramebuffer fb;
    VkPipelineLayout gpl;
    VkPipeline gpipe;
} V;

static int opt_cq = 1;
static uint32_t opt_gq = 1, opt_reps = 7, opt_wgs = 4, opt_px = 8, opt_trace_only = 0;
static double opt_spin_us = 8000;
static const char *opt_cases = "gfx,comp,both,cc,gg,g1c";
static uint32_t ticks_c = 1000000, ticks_g = 1000000;

static uint32_t memtype(uint32_t bits, VkMemoryPropertyFlags want)
{
    VkPhysicalDeviceMemoryProperties mp;
    vkGetPhysicalDeviceMemoryProperties(V.pd, &mp);
    for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
        if ((bits & (1u << i)) && (mp.memoryTypes[i].propertyFlags & want) == want)
            return i;
    fprintf(stderr, "no memory type\n");
    exit(2);
}

static VkShaderModule shader(const unsigned char *code, size_t size)
{
    VkShaderModuleCreateInfo ci = { .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO, .codeSize = size,
                                    .pCode = (const uint32_t *)code };
    VkShaderModule m;
    CHECK(vkCreateShaderModule(V.dev, &ci, NULL, &m));
    return m;
}

static void create_device(void)
{
    VkApplicationInfo ai = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO, .pApplicationName = "vkqtest",
                             .apiVersion = VK_API_VERSION_1_3 };
    VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &ai };
    mark("vkCreateInstance");
    CHECK(vkCreateInstance(&ici, NULL, &V.inst));
    uint32_t n = 8;
    VkPhysicalDevice pds[8];
    CHECK(vkEnumeratePhysicalDevices(V.inst, &n, pds));
    for (uint32_t i = 0; i < n && !V.pd; i++) {
        VkPhysicalDeviceProperties p;
        vkGetPhysicalDeviceProperties(pds[i], &p);
        printf("device %u: %s vendor 0x%04x driver 0x%08x api %u.%u.%u\n", i, p.deviceName, p.vendorID,
               p.driverVersion, VK_VERSION_MAJOR(p.apiVersion), VK_VERSION_MINOR(p.apiVersion),
               VK_VERSION_PATCH(p.apiVersion));
        if (p.vendorID == 0x10de) {
            V.pd = pds[i];
            V.tsperiod = p.limits.timestampPeriod;
        }
    }
    if (!V.pd) {
        fprintf(stderr, "no NVIDIA device\n");
        exit(2);
    }
    VkQueueFamilyProperties qf[16];
    n = 16;
    vkGetPhysicalDeviceQueueFamilyProperties(V.pd, &n, qf);
    V.cfam = UINT32_MAX;
    for (uint32_t i = 0; i < n; i++) {
        printf("queue family %u: count %u flags 0x%x%s%s%s%s timestampValidBits %u\n", i, qf[i].queueCount,
               qf[i].queueFlags, (qf[i].queueFlags & VK_QUEUE_GRAPHICS_BIT) ? " GRAPHICS" : "",
               (qf[i].queueFlags & VK_QUEUE_COMPUTE_BIT) ? " COMPUTE" : "",
               (qf[i].queueFlags & VK_QUEUE_TRANSFER_BIT) ? " TRANSFER" : "",
               (qf[i].queueFlags & VK_QUEUE_SPARSE_BINDING_BIT) ? " SPARSE" : "", qf[i].timestampValidBits);
        if (V.cfam == UINT32_MAX && (qf[i].queueFlags & VK_QUEUE_COMPUTE_BIT) &&
            !(qf[i].queueFlags & VK_QUEUE_GRAPHICS_BIT))
            V.cfam = i;
    }
    V.gfam = 0;
    if (!(qf[0].queueFlags & VK_QUEUE_GRAPHICS_BIT) || V.cfam == UINT32_MAX) {
        fprintf(stderr, "need graphics family 0 and a compute-only family\n");
        exit(2);
    }
    V.ngq = opt_gq < qf[V.gfam].queueCount ? opt_gq : qf[V.gfam].queueCount;
    V.ncq = (uint32_t)opt_cq < qf[V.cfam].queueCount ? (uint32_t)opt_cq : qf[V.cfam].queueCount;
    printf("using graphics family %u x%u, compute family %u x%u, timestampPeriod %.3f ns\n", V.gfam, V.ngq, V.cfam,
           V.ncq, V.tsperiod);

    float prio[MAXQ];
    for (int i = 0; i < MAXQ; i++)
        prio[i] = 1.0f;
    VkDeviceQueueCreateInfo qci[2] = {
        { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueFamilyIndex = V.gfam, .queueCount = V.ngq,
          .pQueuePriorities = prio },
        { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueFamilyIndex = V.cfam, .queueCount = V.ncq,
          .pQueuePriorities = prio },
    };
    VkPhysicalDeviceShaderClockFeaturesKHR clk = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_SHADER_CLOCK_FEATURES_KHR,
                                                   .shaderDeviceClock = VK_TRUE };
    VkPhysicalDeviceVulkan12Features f12 = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES,
                                             .pNext = &clk, .timelineSemaphore = VK_TRUE, .hostQueryReset = VK_TRUE };
    const char *exts[] = { VK_KHR_SHADER_CLOCK_EXTENSION_NAME };
    VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .pNext = &f12, .queueCreateInfoCount = V.ncq ? 2 : 1,
                               .pQueueCreateInfos = qci, .enabledExtensionCount = 1, .ppEnabledExtensionNames = exts };
    mark("vkCreateDevice begin");
    CHECK(vkCreateDevice(V.pd, &dci, NULL, &V.dev));
    mark("vkCreateDevice end");
    char w[96];
    for (uint32_t i = 0; i < V.ngq; i++) {
        snprintf(w, sizeof w, "vkGetDeviceQueue graphics family %u index %u", V.gfam, i);
        mark(w);
        vkGetDeviceQueue(V.dev, V.gfam, i, &V.gq[i]);
    }
    for (uint32_t i = 0; i < V.ncq; i++) {
        snprintf(w, sizeof w, "vkGetDeviceQueue compute family %u index %u", V.cfam, i);
        mark(w);
        vkGetDeviceQueue(V.dev, V.cfam, i, &V.cq[i]);
    }
    mark("queues fetched");
}

static void create_resources(void)
{
    VkCommandPoolCreateInfo pci = { .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
                                    .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT };
    pci.queueFamilyIndex = V.gfam;
    CHECK(vkCreateCommandPool(V.dev, &pci, NULL, &V.gpool));
    pci.queueFamilyIndex = V.cfam;
    CHECK(vkCreateCommandPool(V.dev, &pci, NULL, &V.cpool));

    VkQueryPoolCreateInfo qpi = { .sType = VK_STRUCTURE_TYPE_QUERY_POOL_CREATE_INFO,
                                  .queryType = VK_QUERY_TYPE_TIMESTAMP, .queryCount = 8 };
    CHECK(vkCreateQueryPool(V.dev, &qpi, NULL, &V.qp));

    VkSemaphoreTypeCreateInfo sti = { .sType = VK_STRUCTURE_TYPE_SEMAPHORE_TYPE_CREATE_INFO,
                                      .semaphoreType = VK_SEMAPHORE_TYPE_TIMELINE };
    VkSemaphoreCreateInfo sci = { .sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO, .pNext = &sti };
    CHECK(vkCreateSemaphore(V.dev, &sci, NULL, &V.gate));

    /* compute output buffer */
    VkBufferCreateInfo bci = { .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size = 65536,
                               .usage = VK_BUFFER_USAGE_STORAGE_BUFFER_BIT };
    CHECK(vkCreateBuffer(V.dev, &bci, NULL, &V.buf));
    VkMemoryRequirements mr;
    vkGetBufferMemoryRequirements(V.dev, V.buf, &mr);
    VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = mr.size,
                                 .memoryTypeIndex = memtype(mr.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT) };
    CHECK(vkAllocateMemory(V.dev, &mai, NULL, &V.bufmem));
    CHECK(vkBindBufferMemory(V.dev, V.buf, V.bufmem, 0));

    VkDescriptorSetLayoutBinding b = { .binding = 0, .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
                                       .descriptorCount = 1, .stageFlags = VK_SHADER_STAGE_COMPUTE_BIT };
    VkDescriptorSetLayoutCreateInfo dli = { .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
                                            .bindingCount = 1, .pBindings = &b };
    CHECK(vkCreateDescriptorSetLayout(V.dev, &dli, NULL, &V.dsl));
    VkDescriptorPoolSize ps = { VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 1 };
    VkDescriptorPoolCreateInfo dpi = { .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO, .maxSets = 1,
                                       .poolSizeCount = 1, .pPoolSizes = &ps };
    CHECK(vkCreateDescriptorPool(V.dev, &dpi, NULL, &V.dpool));
    VkDescriptorSetAllocateInfo dai = { .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
                                        .descriptorPool = V.dpool, .descriptorSetCount = 1, .pSetLayouts = &V.dsl };
    CHECK(vkAllocateDescriptorSets(V.dev, &dai, &V.ds));
    VkDescriptorBufferInfo dbi = { V.buf, 0, VK_WHOLE_SIZE };
    VkWriteDescriptorSet wds = { .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = V.ds, .descriptorCount = 1,
                                 .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, .pBufferInfo = &dbi };
    vkUpdateDescriptorSets(V.dev, 1, &wds, 0, NULL);

    VkPushConstantRange pcr = { VK_SHADER_STAGE_COMPUTE_BIT, 0, 4 };
    VkPipelineLayoutCreateInfo pli = { .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO, .setLayoutCount = 1,
                                       .pSetLayouts = &V.dsl, .pushConstantRangeCount = 1,
                                       .pPushConstantRanges = &pcr };
    CHECK(vkCreatePipelineLayout(V.dev, &pli, NULL, &V.cpl));
    VkComputePipelineCreateInfo cpi = {
        .sType = VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO,
        .stage = { .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, .stage = VK_SHADER_STAGE_COMPUTE_BIT,
                   .module = shader(spin_comp_spv, sizeof spin_comp_spv), .pName = "main" },
        .layout = V.cpl,
    };
    CHECK(vkCreateComputePipelines(V.dev, VK_NULL_HANDLE, 1, &cpi, NULL, &V.cpipe));

    /* graphics: 64x64 RGBA8 target, 8x8-pixel viewport by default */
    VkImageCreateInfo ii = { .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO, .imageType = VK_IMAGE_TYPE_2D,
                             .format = VK_FORMAT_R8G8B8A8_UNORM, .extent = { 64, 64, 1 }, .mipLevels = 1,
                             .arrayLayers = 1, .samples = VK_SAMPLE_COUNT_1_BIT, .tiling = VK_IMAGE_TILING_OPTIMAL,
                             .usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT };
    CHECK(vkCreateImage(V.dev, &ii, NULL, &V.img));
    vkGetImageMemoryRequirements(V.dev, V.img, &mr);
    mai.allocationSize = mr.size;
    mai.memoryTypeIndex = memtype(mr.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT);
    CHECK(vkAllocateMemory(V.dev, &mai, NULL, &V.imgmem));
    CHECK(vkBindImageMemory(V.dev, V.img, V.imgmem, 0));
    VkImageViewCreateInfo vi = { .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO, .image = V.img,
                                 .viewType = VK_IMAGE_VIEW_TYPE_2D, .format = VK_FORMAT_R8G8B8A8_UNORM,
                                 .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
    CHECK(vkCreateImageView(V.dev, &vi, NULL, &V.view));
    VkAttachmentDescription ad = { .format = VK_FORMAT_R8G8B8A8_UNORM, .samples = VK_SAMPLE_COUNT_1_BIT,
                                   .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR, .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
                                   .stencilLoadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE,
                                   .stencilStoreOp = VK_ATTACHMENT_STORE_OP_DONT_CARE,
                                   .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
                                   .finalLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL };
    VkAttachmentReference ar = { 0, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL };
    VkSubpassDescription sd = { .pipelineBindPoint = VK_PIPELINE_BIND_POINT_GRAPHICS, .colorAttachmentCount = 1,
                                .pColorAttachments = &ar };
    VkRenderPassCreateInfo rpi = { .sType = VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO, .attachmentCount = 1,
                                   .pAttachments = &ad, .subpassCount = 1, .pSubpasses = &sd };
    CHECK(vkCreateRenderPass(V.dev, &rpi, NULL, &V.rp));
    VkFramebufferCreateInfo fbi = { .sType = VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO, .renderPass = V.rp,
                                    .attachmentCount = 1, .pAttachments = &V.view, .width = 64, .height = 64,
                                    .layers = 1 };
    CHECK(vkCreateFramebuffer(V.dev, &fbi, NULL, &V.fb));
    VkPushConstantRange gpcr = { VK_SHADER_STAGE_FRAGMENT_BIT, 0, 4 };
    VkPipelineLayoutCreateInfo gpli = { .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
                                        .pushConstantRangeCount = 1, .pPushConstantRanges = &gpcr };
    CHECK(vkCreatePipelineLayout(V.dev, &gpli, NULL, &V.gpl));
    VkPipelineShaderStageCreateInfo st[2] = {
        { .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, .stage = VK_SHADER_STAGE_VERTEX_BIT,
          .module = shader(tri_vert_spv, sizeof tri_vert_spv), .pName = "main" },
        { .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, .stage = VK_SHADER_STAGE_FRAGMENT_BIT,
          .module = shader(spin_frag_spv, sizeof spin_frag_spv), .pName = "main" },
    };
    VkPipelineVertexInputStateCreateInfo vin = { .sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO };
    VkPipelineInputAssemblyStateCreateInfo ia = { .sType = VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
                                                  .topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST };
    VkPipelineViewportStateCreateInfo vps = { .sType = VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO,
                                              .viewportCount = 1, .scissorCount = 1 };
    VkPipelineRasterizationStateCreateInfo rs = { .sType = VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO,
                                                  .polygonMode = VK_POLYGON_MODE_FILL, .cullMode = VK_CULL_MODE_NONE,
                                                  .lineWidth = 1.0f };
    VkPipelineMultisampleStateCreateInfo ms = { .sType = VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO,
                                                .rasterizationSamples = VK_SAMPLE_COUNT_1_BIT };
    VkPipelineColorBlendAttachmentState cba = { .colorWriteMask = 0xf };
    VkPipelineColorBlendStateCreateInfo cb = { .sType = VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO,
                                               .attachmentCount = 1, .pAttachments = &cba };
    VkDynamicState dyn[2] = { VK_DYNAMIC_STATE_VIEWPORT, VK_DYNAMIC_STATE_SCISSOR };
    VkPipelineDynamicStateCreateInfo ds = { .sType = VK_STRUCTURE_TYPE_PIPELINE_DYNAMIC_STATE_CREATE_INFO,
                                            .dynamicStateCount = 2, .pDynamicStates = dyn };
    VkGraphicsPipelineCreateInfo gpi = { .sType = VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO, .stageCount = 2,
                                         .pStages = st, .pVertexInputState = &vin, .pInputAssemblyState = &ia,
                                         .pViewportState = &vps, .pRasterizationState = &rs,
                                         .pMultisampleState = &ms, .pColorBlendState = &cb, .pDynamicState = &ds,
                                         .layout = V.gpl, .renderPass = V.rp };
    CHECK(vkCreateGraphicsPipelines(V.dev, VK_NULL_HANDLE, 1, &gpi, NULL, &V.gpipe));
}

enum { W_GFX, W_COMP };

/* record one command buffer: timestamp, work, timestamp (queries q0, q0+1) */
static VkCommandBuffer record(int kind, uint32_t q0, uint32_t ticks)
{
    VkCommandBufferAllocateInfo cai = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
                                        .commandPool = kind == W_GFX ? V.gpool : V.cpool,
                                        .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1 };
    VkCommandBuffer cb;
    CHECK(vkAllocateCommandBuffers(V.dev, &cai, &cb));
    VkCommandBufferBeginInfo bi = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
                                    .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT };
    CHECK(vkBeginCommandBuffer(cb, &bi));
    vkCmdWriteTimestamp(cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, V.qp, q0);
    if (kind == W_COMP) {
        vkCmdBindPipeline(cb, VK_PIPELINE_BIND_POINT_COMPUTE, V.cpipe);
        vkCmdBindDescriptorSets(cb, VK_PIPELINE_BIND_POINT_COMPUTE, V.cpl, 0, 1, &V.ds, 0, NULL);
        vkCmdPushConstants(cb, V.cpl, VK_SHADER_STAGE_COMPUTE_BIT, 0, 4, &ticks);
        vkCmdDispatch(cb, opt_wgs, 1, 1);
    } else {
        VkClearValue cv = { .color = { { 0, 0, 0, 1 } } };
        VkRenderPassBeginInfo rbi = { .sType = VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO, .renderPass = V.rp,
                                      .framebuffer = V.fb, .renderArea = { { 0, 0 }, { 64, 64 } },
                                      .clearValueCount = 1, .pClearValues = &cv };
        vkCmdBeginRenderPass(cb, &rbi, VK_SUBPASS_CONTENTS_INLINE);
        vkCmdBindPipeline(cb, VK_PIPELINE_BIND_POINT_GRAPHICS, V.gpipe);
        VkViewport vp = { 0, 0, (float)opt_px, (float)opt_px, 0, 1 };
        VkRect2D sc = { { 0, 0 }, { opt_px, opt_px } };
        vkCmdSetViewport(cb, 0, 1, &vp);
        vkCmdSetScissor(cb, 0, 1, &sc);
        vkCmdPushConstants(cb, V.gpl, VK_SHADER_STAGE_FRAGMENT_BIT, 0, 4, &ticks);
        vkCmdDraw(cb, 3, 1, 0, 0);
        vkCmdEndRenderPass(cb);
    }
    vkCmdWriteTimestamp(cb, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, V.qp, q0 + 1);
    CHECK(vkEndCommandBuffer(cb));
    return cb;
}

struct job {
    int kind;
    VkQueue q;
};

/* run jobs (1 or 2) gated together; returns start/end ns per job */
static void run_jobs(const struct job *jobs, int nj, double *start, double *end)
{
    VkCommandBuffer cbs[2];
    VkFence fences[2];
    vkResetQueryPool(V.dev, V.qp, 0, 8);
    uint64_t gv = ++V.gate_val;
    for (int i = 0; i < nj; i++) {
        cbs[i] = record(jobs[i].kind, 2 * i, jobs[i].kind == W_GFX ? ticks_g : ticks_c);
        VkFenceCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
        CHECK(vkCreateFence(V.dev, &fci, NULL, &fences[i]));
        VkTimelineSemaphoreSubmitInfo tsi = { .sType = VK_STRUCTURE_TYPE_TIMELINE_SEMAPHORE_SUBMIT_INFO,
                                              .waitSemaphoreValueCount = 1, .pWaitSemaphoreValues = &gv };
        VkPipelineStageFlags ws = VK_PIPELINE_STAGE_ALL_COMMANDS_BIT;
        VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .pNext = &tsi, .waitSemaphoreCount = 1,
                            .pWaitSemaphores = &V.gate, .pWaitDstStageMask = &ws, .commandBufferCount = 1,
                            .pCommandBuffers = &cbs[i] };
        CHECK(vkQueueSubmit(jobs[i].q, 1, &si, fences[i]));
    }
    VkSemaphoreSignalInfo ssi = { .sType = VK_STRUCTURE_TYPE_SEMAPHORE_SIGNAL_INFO, .semaphore = V.gate, .value = gv };
    CHECK(vkSignalSemaphore(V.dev, &ssi));
    CHECK(vkWaitForFences(V.dev, nj, fences, VK_TRUE, 5000000000ull));
    uint64_t ts[4];
    CHECK(vkGetQueryPoolResults(V.dev, V.qp, 0, 2 * nj, sizeof ts, ts, 8,
                                VK_QUERY_RESULT_64_BIT | VK_QUERY_RESULT_WAIT_BIT));
    for (int i = 0; i < nj; i++) {
        start[i] = ts[2 * i] * (double)V.tsperiod;
        end[i] = ts[2 * i + 1] * (double)V.tsperiod;
        vkDestroyFence(V.dev, fences[i], NULL);
        vkFreeCommandBuffers(V.dev, jobs[i].kind == W_GFX ? V.gpool : V.cpool, 1, &cbs[i]);
    }
}

static int cmpd(const void *a, const void *b)
{
    double x = *(const double *)a, y = *(const double *)b;
    return x < y ? -1 : x > y;
}
static double median(double *v, int n)
{
    qsort(v, n, sizeof *v, cmpd);
    return n % 2 ? v[n / 2] : 0.5 * (v[n / 2 - 1] + v[n / 2]);
}

static double solo(int kind, VkQueue q, const char *name)
{
    double d[64];
    for (uint32_t r = 0; r < opt_reps; r++) {
        struct job j = { kind, q };
        double s, e;
        run_jobs(&j, 1, &s, &e);
        d[r] = (e - s) / 1e3;
    }
    double m = median(d, opt_reps);
    printf("solo %-6s median %9.1f us\n", name, m);
    return m;
}

static void pair(const char *name, struct job a, struct job b, double sa, double sb)
{
    double mk[64], sk[64];
    for (uint32_t r = 0; r < opt_reps; r++) {
        struct job j[2] = { a, b };
        double s[2], e[2];
        run_jobs(j, 2, s, e);
        double t0 = s[0] < s[1] ? s[0] : s[1], t1 = e[0] > e[1] ? e[0] : e[1];
        mk[r] = (t1 - t0) / 1e3;
        sk[r] = fabs(s[0] - s[1]) / 1e3;
        printf("  %s rep %u: A %.1f..%.1f us  B %.1f..%.1f us  makespan %.1f us\n", name, r, 0.0 + (s[0] - t0) / 1e3,
               (e[0] - t0) / 1e3, (s[1] - t0) / 1e3, (e[1] - t0) / 1e3, mk[r]);
    }
    double m = median(mk, opt_reps), skew = median(sk, opt_reps);
    double lo = sa < sb ? sa : sb, hi = sa > sb ? sa : sb;
    double ov = 1.0 - (m - hi) / lo;
    if (ov > 1)
        ov = 1;
    if (ov < 0)
        ov = 0;
    const char *verdict = ov >= 0.75 ? "CONCURRENT" : ov <= 0.25 ? "SERIAL" : "PARTIAL";
    printf("RESULT %-5s solo %.1f/%.1f us  makespan %.1f us  sum %.1f us  start skew %.1f us  overlap %.0f%%  %s\n",
           name, sa, sb, m, sa + sb, skew, ov * 100, verdict);
}

static int has_case(const char *c)
{
    char buf[256];
    snprintf(buf, sizeof buf, ",%s,", opt_cases);
    char k[32];
    snprintf(k, sizeof k, ",%s,", c);
    return strstr(buf, k) != NULL;
}

static uint32_t calibrate(int kind, VkQueue q, const char *name)
{
    uint32_t *t = kind == W_GFX ? &ticks_g : &ticks_c;
    *t = 1000000;
    struct job j = { kind, q };
    double s, e;
    run_jobs(&j, 1, &s, &e); /* warm-up */
    run_jobs(&j, 1, &s, &e);
    double us = (e - s) / 1e3;
    double scale = opt_spin_us / us;
    double nt = *t * scale;
    if (nt > 2e9)
        nt = 2e9;
    *t = (uint32_t)nt;
    printf("calibrate %s: 1e6 ticks -> %.1f us; using %u ticks for %.0f us\n", name, us, *t, opt_spin_us);
    return *t;
}

int main(int argc, char **argv)
{
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--cq") && i + 1 < argc)
            opt_cq = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--gq") && i + 1 < argc)
            opt_gq = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--spin-us") && i + 1 < argc)
            opt_spin_us = atof(argv[++i]);
        else if (!strcmp(argv[i], "--reps") && i + 1 < argc)
            opt_reps = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--wgs") && i + 1 < argc)
            opt_wgs = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--px") && i + 1 < argc)
            opt_px = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--cases") && i + 1 < argc)
            opt_cases = argv[++i];
        else if (!strcmp(argv[i], "--trace-only"))
            opt_trace_only = 1;
        else {
            fprintf(stderr, "usage: %s [--cq n] [--gq n] [--spin-us us] [--reps n] [--wgs n] [--px n] "
                            "[--cases gfx,comp,both,cc,gg,g1c] [--trace-only]\n", argv[0]);
            return 2;
        }
    }
    if (opt_reps < 1)
        opt_reps = 1;
    if (opt_reps > 64)
        opt_reps = 64;
    if (opt_gq < 1)
        opt_gq = 1;
    if (opt_cq < 0)
        opt_cq = 0;
    if (opt_cq > MAXQ)
        opt_cq = MAXQ;
    if (opt_gq > MAXQ)
        opt_gq = MAXQ;
    mark_fn = (void (*)(const char *))dlsym(RTLD_DEFAULT, "rmtrace_mark");

    create_device();
    mark("create resources");
    create_resources();

    /* first submit on every queue (channels may be created lazily) */
    char w[96];
    ticks_c = ticks_g = 1000;
    for (uint32_t i = 0; i < V.ngq; i++) {
        snprintf(w, sizeof w, "first submit graphics queue %u", i);
        mark(w);
        struct job j = { W_GFX, V.gq[i] };
        double s, e;
        run_jobs(&j, 1, &s, &e);
    }
    for (uint32_t i = 0; i < V.ncq; i++) {
        snprintf(w, sizeof w, "first submit compute queue %u", i);
        mark(w);
        struct job j = { W_COMP, V.cq[i] };
        double s, e;
        run_jobs(&j, 1, &s, &e);
    }
    mark("first submits done");
    if (opt_trace_only || V.ncq == 0)
        goto out;

    mark("calibrate");
    calibrate(W_COMP, V.cq[0], "compute");
    calibrate(W_GFX, V.gq[0], "graphics");
    double sg = 0, sc = 0;
    if (has_case("gfx") || has_case("both") || has_case("gg"))
        sg = solo(W_GFX, V.gq[0], "gfx");
    if (has_case("comp") || has_case("both") || has_case("cc"))
        sc = solo(W_COMP, V.cq[0], "comp");
    if (has_case("both")) {
        mark("case both");
        pair("both", (struct job){ W_GFX, V.gq[0] }, (struct job){ W_COMP, V.cq[0] }, sg, sc);
    }
    if (has_case("cc") && V.ncq >= 2) {
        mark("case cc");
        double sc1 = solo(W_COMP, V.cq[1], "comp1");
        pair("cc", (struct job){ W_COMP, V.cq[0] }, (struct job){ W_COMP, V.cq[1] }, sc, sc1);
    }
    if (has_case("gg") && V.ngq >= 2) {
        mark("case gg");
        double sg1 = solo(W_GFX, V.gq[1], "gfx1");
        pair("gg", (struct job){ W_GFX, V.gq[0] }, (struct job){ W_GFX, V.gq[1] }, sg, sg1);
    }
    if (has_case("both") && V.ncq >= 2) {
        mark("case both (compute queue 1)");
        pair("both1", (struct job){ W_GFX, V.gq[0] }, (struct job){ W_COMP, V.cq[1] }, sg, sc);
    }
    if (has_case("g1c") && V.ngq >= 2) {
        mark("case g1c (graphics queue 1 + compute queue 0)");
        double sg1 = solo(W_GFX, V.gq[1], "gfx1");
        pair("g1c", (struct job){ W_GFX, V.gq[1] }, (struct job){ W_COMP, V.cq[0] }, sg1, sc);
    }
out:
    mark("vkDestroyDevice");
    vkDeviceWaitIdle(V.dev);
    vkDestroyDevice(V.dev, NULL); /* objects leak with the device; fine for a test */
    vkDestroyInstance(V.inst, NULL);
    mark("done");
    return 0;
}
