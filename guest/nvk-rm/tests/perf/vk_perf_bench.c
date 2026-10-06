/*
 * vk_perf_bench: GPU time per workload category, measured with timestamp
 * queries, to compare Vulkan drivers on the same GPU (NVK on RM vs NVIDIA).
 *
 *   VK_DRIVER_FILES=<icd.json> ./vk_perf_bench [-r reps] [-t test,...]
 *
 * Offscreen only (no WSI).  Each test records one command buffer bracketed by
 * timestamps, submits it reps times (after one warm-up) and prints the median.
 */
#include <vulkan/vulkan.h>

#include <assert.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include "shaders/alu_frag.h"
#include "shaders/blend_frag.h"
#include "shaders/color_frag.h"
#include "shaders/colortex_frag.h"
#include "shaders/copy_comp.h"
#include "shaders/fsq_vert.h"
#include "shaders/fsqz_vert.h"
#include "shaders/mesh_vert.h"
#include "shaders/meshubo_vert.h"
#include "shaders/tess_tesc.h"
#include "shaders/tess_tese.h"
#include "shaders/tess_vert.h"
#include "shaders/tri_vert.h"
#include "shaders/tri_tesc.h"
#include "shaders/tri_tese.h"
#include "shaders/tex_frag.h"
#include "shaders/ubo_vert.h"

#define CHECK(x)                                                              \
   do {                                                                       \
      VkResult r_ = (x);                                                      \
      if (r_ != VK_SUCCESS) {                                                 \
         fprintf(stderr, "%s:%d: %s = %d\n", __FILE__, __LINE__, #x, r_);     \
         exit(1);                                                             \
      }                                                                       \
   } while (0)

#define W 2560
#define H 1440
#define TEX_DIM 4096

static VkInstance inst;
static VkPhysicalDevice pdev;
static VkDevice dev;
static VkQueue queue;
static uint32_t qfam;
static VkPhysicalDeviceMemoryProperties memprops;
static VkPhysicalDeviceProperties props;
static VkCommandPool cpool;
static VkQueryPool qpool;
static int reps = 20;

/* ------------------------------------------------------------------ */

enum placement { PL_DEVICE, PL_HOST, PL_DEVICE_HOST };

static int
find_memtype(uint32_t bits, VkMemoryPropertyFlags want, VkMemoryPropertyFlags avoid)
{
   for (uint32_t i = 0; i < memprops.memoryTypeCount; i++) {
      VkMemoryPropertyFlags f = memprops.memoryTypes[i].propertyFlags;
      if ((bits & (1u << i)) && (f & want) == want && !(f & avoid))
         return i;
   }
   return -1;
}

static int
placement_type(uint32_t bits, enum placement pl)
{
   switch (pl) {
   case PL_DEVICE:
      return find_memtype(bits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT,
                          VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT);
   case PL_HOST:
      return find_memtype(bits, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                                VK_MEMORY_PROPERTY_HOST_COHERENT_BIT,
                          VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT);
   case PL_DEVICE_HOST:
      return find_memtype(bits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT |
                                VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                                VK_MEMORY_PROPERTY_HOST_COHERENT_BIT, 0);
   }
   return -1;
}

struct buf {
   VkBuffer buf;
   VkDeviceMemory mem;
   void *map;
   VkDeviceSize size;
};

static bool
make_buffer(struct buf *b, VkDeviceSize size, VkBufferUsageFlags usage,
            enum placement pl)
{
   memset(b, 0, sizeof(*b));
   b->size = size;
   VkBufferCreateInfo bci = {
      .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
      .size = size,
      .usage = usage | VK_BUFFER_USAGE_TRANSFER_DST_BIT |
               VK_BUFFER_USAGE_TRANSFER_SRC_BIT,
   };
   CHECK(vkCreateBuffer(dev, &bci, NULL, &b->buf));
   VkMemoryRequirements req;
   vkGetBufferMemoryRequirements(dev, b->buf, &req);
   int mt = placement_type(req.memoryTypeBits, pl);
   if (mt < 0) {
      vkDestroyBuffer(dev, b->buf, NULL);
      b->buf = VK_NULL_HANDLE;
      return false;
   }
   VkMemoryAllocateInfo mai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .allocationSize = req.size,
      .memoryTypeIndex = mt,
   };
   CHECK(vkAllocateMemory(dev, &mai, NULL, &b->mem));
   CHECK(vkBindBufferMemory(dev, b->buf, b->mem, 0));
   if (memprops.memoryTypes[mt].propertyFlags & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT)
      CHECK(vkMapMemory(dev, b->mem, 0, VK_WHOLE_SIZE, 0, &b->map));
   return true;
}

static void
free_buffer(struct buf *b)
{
   if (b->buf == VK_NULL_HANDLE)
      return;
   vkDestroyBuffer(dev, b->buf, NULL);
   vkFreeMemory(dev, b->mem, NULL);
   memset(b, 0, sizeof(*b));
}

struct img {
   VkImage img;
   VkDeviceMemory mem;
   VkImageView view;
   VkFormat fmt;
};

static void
make_image(struct img *im, VkFormat fmt, uint32_t w, uint32_t h,
           VkImageUsageFlags usage, VkImageAspectFlags aspect)
{
   im->fmt = fmt;
   VkImageCreateInfo ici = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
      .imageType = VK_IMAGE_TYPE_2D,
      .format = fmt,
      .extent = { w, h, 1 },
      .mipLevels = 1,
      .arrayLayers = 1,
      .samples = VK_SAMPLE_COUNT_1_BIT,
      .tiling = VK_IMAGE_TILING_OPTIMAL,
      .usage = usage,
      .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
   };
   CHECK(vkCreateImage(dev, &ici, NULL, &im->img));
   VkMemoryRequirements req;
   vkGetImageMemoryRequirements(dev, im->img, &req);
   /* Dedicated, as DXVK does for render targets (and NVK needs for
    * compression) */
   VkMemoryDedicatedAllocateInfo ded = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
      .image = im->img,
   };
   VkMemoryAllocateInfo mai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .pNext = getenv("BENCH_NO_DEDICATED") ? NULL : &ded,
      .allocationSize = req.size,
      .memoryTypeIndex = placement_type(req.memoryTypeBits, PL_DEVICE),
   };
   CHECK(vkAllocateMemory(dev, &mai, NULL, &im->mem));
   CHECK(vkBindImageMemory(dev, im->img, im->mem, 0));
   VkImageViewCreateInfo vci = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
      .image = im->img,
      .viewType = VK_IMAGE_VIEW_TYPE_2D,
      .format = fmt,
      .subresourceRange = { aspect, 0, 1, 0, 1 },
   };
   CHECK(vkCreateImageView(dev, &vci, NULL, &im->view));
}

static VkShaderModule
shader(const uint32_t *code, size_t size)
{
   VkShaderModuleCreateInfo ci = {
      .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
      .codeSize = size,
      .pCode = code,
   };
   VkShaderModule m;
   CHECK(vkCreateShaderModule(dev, &ci, NULL, &m));
   return m;
}
#define SHADER(x) shader(x, sizeof(x))

/* ------------------------------------------------------------------ */

static VkDescriptorSetLayout dsl_gfx, dsl_comp;
static VkPipelineLayout pl_gfx, pl_comp;
static VkDescriptorPool dpool;

struct gfx_desc {
   VkShaderModule vs, tcs, tes, fs;
   VkFormat color;
   bool blend, depth, mesh_vb, patch3;
   VkPrimitiveTopology topo;
};

static VkPipeline
make_gfx(const struct gfx_desc *d)
{
   VkPipelineShaderStageCreateInfo st[4];
   uint32_t n = 0;
#define STAGE(m, s)                                                           \
   if (m) st[n++] = (VkPipelineShaderStageCreateInfo){                        \
      .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,          \
      .stage = s, .module = m, .pName = "main" }
   STAGE(d->vs, VK_SHADER_STAGE_VERTEX_BIT);
   STAGE(d->tcs, VK_SHADER_STAGE_TESSELLATION_CONTROL_BIT);
   STAGE(d->tes, VK_SHADER_STAGE_TESSELLATION_EVALUATION_BIT);
   STAGE(d->fs, VK_SHADER_STAGE_FRAGMENT_BIT);

   VkVertexInputBindingDescription vb = { 0, 16, VK_VERTEX_INPUT_RATE_VERTEX };
   VkVertexInputAttributeDescription va = { 0, 0, VK_FORMAT_R32G32B32A32_SFLOAT, 0 };
   VkPipelineVertexInputStateCreateInfo vi = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO,
      .vertexBindingDescriptionCount = d->mesh_vb ? 1 : 0,
      .pVertexBindingDescriptions = &vb,
      .vertexAttributeDescriptionCount = d->mesh_vb ? 1 : 0,
      .pVertexAttributeDescriptions = &va,
   };
   VkPipelineInputAssemblyStateCreateInfo ia = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
      .topology = d->topo,
   };
   VkPipelineTessellationStateCreateInfo ts = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_TESSELLATION_STATE_CREATE_INFO,
      .patchControlPoints = d->patch3 ? 3 : 4,
   };
   VkViewport vp = { 0, 0, W, H, 0, 1 };
   VkRect2D sc = { { 0, 0 }, { W, H } };
   VkPipelineViewportStateCreateInfo vps = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO,
      .viewportCount = 1, .pViewports = &vp,
      .scissorCount = 1, .pScissors = &sc,
   };
   VkPipelineRasterizationStateCreateInfo rs = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO,
      .polygonMode = VK_POLYGON_MODE_FILL,
      .cullMode = VK_CULL_MODE_NONE,
      .lineWidth = 1.0f,
   };
   VkPipelineMultisampleStateCreateInfo ms = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO,
      .rasterizationSamples = VK_SAMPLE_COUNT_1_BIT,
   };
   VkPipelineDepthStencilStateCreateInfo ds = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_DEPTH_STENCIL_STATE_CREATE_INFO,
      .depthTestEnable = d->depth,
      .depthWriteEnable = d->depth,
      .depthCompareOp = VK_COMPARE_OP_LESS_OR_EQUAL,
   };
   VkPipelineColorBlendAttachmentState cba = {
      .blendEnable = d->blend,
      .srcColorBlendFactor = VK_BLEND_FACTOR_SRC_ALPHA,
      .dstColorBlendFactor = VK_BLEND_FACTOR_ONE_MINUS_SRC_ALPHA,
      .colorBlendOp = VK_BLEND_OP_ADD,
      .srcAlphaBlendFactor = VK_BLEND_FACTOR_ONE,
      .dstAlphaBlendFactor = VK_BLEND_FACTOR_ZERO,
      .alphaBlendOp = VK_BLEND_OP_ADD,
      .colorWriteMask = 0xf,
   };
   VkPipelineColorBlendStateCreateInfo cb = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO,
      .attachmentCount = 1, .pAttachments = &cba,
   };
   VkPipelineRenderingCreateInfo ri = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_RENDERING_CREATE_INFO,
      .colorAttachmentCount = 1,
      .pColorAttachmentFormats = &d->color,
      .depthAttachmentFormat = VK_FORMAT_D32_SFLOAT,
   };
   VkGraphicsPipelineCreateInfo ci = {
      .sType = VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO,
      .pNext = &ri,
      .stageCount = n, .pStages = st,
      .pVertexInputState = &vi,
      .pInputAssemblyState = &ia,
      .pTessellationState = d->tcs ? &ts : NULL,
      .pViewportState = &vps,
      .pRasterizationState = &rs,
      .pMultisampleState = &ms,
      .pDepthStencilState = &ds,
      .pColorBlendState = &cb,
      .layout = pl_gfx,
   };
   VkPipeline p;
   CHECK(vkCreateGraphicsPipelines(dev, VK_NULL_HANDLE, 1, &ci, NULL, &p));
   return p;
}

/* ------------------------------------------------------------------ */

static VkCommandBuffer
begin_cmd(void)
{
   VkCommandBufferAllocateInfo ai = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
      .commandPool = cpool,
      .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
      .commandBufferCount = 1,
   };
   VkCommandBuffer cmd;
   CHECK(vkAllocateCommandBuffers(dev, &ai, &cmd));
   VkCommandBufferBeginInfo bi = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
   };
   CHECK(vkBeginCommandBuffer(cmd, &bi));
   return cmd;
}

static void
submit_wait(VkCommandBuffer cmd)
{
   VkSubmitInfo si = {
      .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
      .commandBufferCount = 1, .pCommandBuffers = &cmd,
   };
   CHECK(vkQueueSubmit(queue, 1, &si, VK_NULL_HANDLE));
   CHECK(vkQueueWaitIdle(queue));
}

static void
barrier_all(VkCommandBuffer cmd)
{
   VkMemoryBarrier mb = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
      .srcAccessMask = VK_ACCESS_MEMORY_WRITE_BIT,
      .dstAccessMask = VK_ACCESS_MEMORY_READ_BIT | VK_ACCESS_MEMORY_WRITE_BIT,
   };
   vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT,
                        VK_PIPELINE_STAGE_ALL_COMMANDS_BIT, 0, 1, &mb, 0, NULL,
                        0, NULL);
}

static void
image_layout(VkCommandBuffer cmd, VkImage img, VkImageAspectFlags aspect,
             VkImageLayout from, VkImageLayout to)
{
   VkImageMemoryBarrier b = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
      .srcAccessMask = VK_ACCESS_MEMORY_WRITE_BIT,
      .dstAccessMask = VK_ACCESS_MEMORY_READ_BIT | VK_ACCESS_MEMORY_WRITE_BIT,
      .oldLayout = from,
      .newLayout = to,
      .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
      .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
      .image = img,
      .subresourceRange = { aspect, 0, 1, 0, 1 },
   };
   vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT,
                        VK_PIPELINE_STAGE_ALL_COMMANDS_BIT, 0, 0, NULL, 0, NULL,
                        1, &b);
}

static double
now_ms(void)
{
   struct timespec ts;
   clock_gettime(CLOCK_MONOTONIC, &ts);
   return ts.tv_sec * 1e3 + ts.tv_nsec / 1e6;
}

static int
cmp_double(const void *a, const void *b)
{
   double x = *(const double *)a, y = *(const double *)b;
   return x < y ? -1 : x > y;
}

/* Submits cmd (which must write timestamps 0 and 1) reps times. */
static double
time_cmd(VkCommandBuffer cmd, double *cpu_ms_out)
{
   double gpu[256], cpu[256];
   int n = reps < 256 ? reps : 256;
   submit_wait(cmd); /* warm-up */
   for (int i = 0; i < n; i++) {
      double t0 = now_ms();
      submit_wait(cmd);
      cpu[i] = now_ms() - t0;
      uint64_t ts[2];
      CHECK(vkGetQueryPoolResults(dev, qpool, 0, 2, sizeof(ts), ts, 8,
                                  VK_QUERY_RESULT_64_BIT |
                                  VK_QUERY_RESULT_WAIT_BIT));
      gpu[i] = (double)(ts[1] - ts[0]) * props.limits.timestampPeriod / 1e6;
   }
   qsort(gpu, n, sizeof(double), cmp_double);
   qsort(cpu, n, sizeof(double), cmp_double);
   if (cpu_ms_out)
      *cpu_ms_out = cpu[n / 2];
   return gpu[n / 2];
}

static void
ts_begin(VkCommandBuffer cmd)
{
   vkCmdResetQueryPool(cmd, qpool, 0, 2);
   vkCmdWriteTimestamp(cmd, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, qpool, 0);
}

static void
ts_end(VkCommandBuffer cmd)
{
   vkCmdWriteTimestamp(cmd, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, qpool, 1);
   CHECK(vkEndCommandBuffer(cmd));
}

/* ------------------------------------------------------------------ */

static struct img rt8, rt8b, rt16, depth, tex;
static VkSampler sampler;
static VkShaderModule m_trivs, m_tritcs, m_trites, m_fsqz, m_fsq, m_alu, m_tex, m_blend, m_ubo, m_color, m_colortex,
   m_mesh, m_meshubo, m_tvs, m_tcs, m_tes, m_copy;

static void
begin_render(VkCommandBuffer cmd, struct img *color, bool clear, bool use_depth)
{
   VkRenderingAttachmentInfo ca = {
      .sType = VK_STRUCTURE_TYPE_RENDERING_ATTACHMENT_INFO,
      .imageView = color->view,
      .imageLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
      .loadOp = clear ? VK_ATTACHMENT_LOAD_OP_CLEAR : VK_ATTACHMENT_LOAD_OP_LOAD,
      .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
   };
   VkRenderingAttachmentInfo da = {
      .sType = VK_STRUCTURE_TYPE_RENDERING_ATTACHMENT_INFO,
      .imageView = depth.view,
      .imageLayout = VK_IMAGE_LAYOUT_DEPTH_ATTACHMENT_OPTIMAL,
      .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR,
      .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
      .clearValue.depthStencil = { 1.0f, 0 },
   };
   VkRenderingInfo ri = {
      .sType = VK_STRUCTURE_TYPE_RENDERING_INFO,
      .renderArea = { { 0, 0 }, { W, H } },
      .layerCount = 1,
      .colorAttachmentCount = 1,
      .pColorAttachments = &ca,
      .pDepthAttachment = use_depth ? &da : NULL,
   };
   vkCmdBeginRendering(cmd, &ri);
}

static VkDescriptorSet
alloc_set(VkDescriptorSetLayout l)
{
   VkDescriptorSetAllocateInfo ai = {
      .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
      .descriptorPool = dpool,
      .descriptorSetCount = 1,
      .pSetLayouts = &l,
   };
   VkDescriptorSet s;
   CHECK(vkAllocateDescriptorSets(dev, &ai, &s));
   return s;
}

static void
write_gfx_set(VkDescriptorSet s, VkBuffer ubo)
{
   VkDescriptorBufferInfo bi = { ubo, 0, 256 };
   VkDescriptorImageInfo ii = { sampler, tex.view,
                                VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL };
   VkWriteDescriptorSet w[2] = {
      { .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = s,
        .dstBinding = 0, .descriptorCount = 1,
        .descriptorType = VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER_DYNAMIC,
        .pBufferInfo = &bi },
      { .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = s,
        .dstBinding = 1, .descriptorCount = 1,
        .descriptorType = VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER,
        .pImageInfo = &ii },
   };
   vkUpdateDescriptorSets(dev, 2, w, 0, NULL);
}

static struct buf dummy_ubo;
static VkDescriptorSet set_default;

static void
report(const char *name, const char *unit, double work, double gpu_ms, double cpu_ms)
{
   if (work > 0)
      printf("%-28s gpu %9.3f ms  cpu %9.3f ms  %10.2f %s\n", name, gpu_ms,
             cpu_ms, work / (gpu_ms * 1e-3) / (unit[0] == 'G' ? 1e9 : 1e6), unit);
   else
      printf("%-28s gpu %9.3f ms  cpu %9.3f ms\n", name, gpu_ms, cpu_ms);
   fflush(stdout);
}

/* fullscreen passes */
static void
test_fullscreen(const char *name, VkPipeline p, struct img *rt, int passes,
                const float pc[4], const char *unit, double work)
{
   VkCommandBuffer cmd = begin_cmd();
   ts_begin(cmd);
   begin_render(cmd, rt, true, false);
   vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, p);
   uint32_t off = 0;
   vkCmdBindDescriptorSets(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, pl_gfx, 0, 1,
                           &set_default, 1, &off);
   vkCmdPushConstants(cmd, pl_gfx, VK_SHADER_STAGE_ALL, 0, 16, pc);
   for (int i = 0; i < passes; i++)
      vkCmdDraw(cmd, 3, 1, 0, 0);
   vkCmdEndRendering(cmd);
   ts_end(cmd);
   double cpu, gpu = time_cmd(cmd, &cpu);
   report(name, unit, work, gpu, cpu);
   vkFreeCommandBuffers(dev, cpool, 1, &cmd);
}

static const char *pl_name[] = { "vidmem", "sysmem", "vidmem+hostvis" };

/* Creates a buffer in placement pl filled with size bytes from data
 * (through a staging copy if it is not mappable). */
static bool
upload_buffer(struct buf *b, const void *data, VkDeviceSize size,
              VkBufferUsageFlags usage, enum placement pl)
{
   if (!make_buffer(b, size, usage, pl))
      return false;
   if (b->map) {
      memcpy(b->map, data, size);
      return true;
   }
   struct buf stage;
   make_buffer(&stage, size, 0, PL_HOST);
   memcpy(stage.map, data, size);
   VkCommandBuffer c = begin_cmd();
   VkBufferCopy r = { 0, 0, size };
   vkCmdCopyBuffer(c, stage.buf, b->buf, 1, &r);
   CHECK(vkEndCommandBuffer(c));
   submit_wait(c);
   vkFreeCommandBuffers(dev, cpool, 1, &c);
   free_buffer(&stage);
   return true;
}

#define UBO_STRIDE 256

static bool
make_draw_ubos(struct buf *ubo, int ndraws, enum placement pl, float scale)
{
   float *data = calloc(ndraws, UBO_STRIDE);
   for (int i = 0; i < ndraws; i++) {
      float *m = data + i * (UBO_STRIDE / 4);
      m[0] = m[5] = m[10] = m[15] = 1.0f;
      m[0] = m[5] = scale;
      m[12] = ((i * 37) % 97) / 48.5f - 1.0f; /* translation x */
      m[13] = ((i * 53) % 89) / 44.5f - 1.0f; /* translation y */
      m[16] = (i & 7) / 7.0f; m[17] = 0.5f; m[18] = 0.25f; m[19] = 1.0f;
   }
   bool ok = upload_buffer(ubo, data, (VkDeviceSize)ndraws * UBO_STRIDE,
                           VK_BUFFER_USAGE_UNIFORM_BUFFER_BIT, pl);
   free(data);
   return ok;
}

static void
reset_sets(void)
{
   CHECK(vkResetDescriptorPool(dev, dpool, 0));
   set_default = alloc_set(dsl_gfx);
   write_gfx_set(set_default, dummy_ubo.buf);
}

/* many small draws, each with its own dynamic UBO offset */
static void
test_ubo_draws(VkPipeline p, enum placement pl, int ndraws, bool switch_sets,
               const char *label)
{
   struct buf ubo;
   if (!make_draw_ubos(&ubo, ndraws, pl, 1.0f)) {
      printf("%-28s (no %s memory type)\n", label, pl_name[pl]);
      return;
   }

   enum { NSETS = 64 };
   VkDescriptorSet sets[NSETS];
   int nsets = switch_sets ? NSETS : 1;
   for (int i = 0; i < nsets; i++) {
      sets[i] = alloc_set(dsl_gfx);
      write_gfx_set(sets[i], ubo.buf);
   }

   VkCommandBuffer cmd = begin_cmd();
   ts_begin(cmd);
   begin_render(cmd, &rt8, true, false);
   vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, p);
   const bool bind_once = label[0] == 'p'; /* "plain draws" */
   for (int i = 0; i < ndraws; i++) {
      uint32_t off = i * UBO_STRIDE;
      if (!bind_once || i == 0)
         vkCmdBindDescriptorSets(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, pl_gfx, 0,
                                 1, &sets[i % nsets], 1, &off);
      vkCmdDraw(cmd, 4, 1, 0, 0);
   }
   vkCmdEndRendering(cmd);
   ts_end(cmd);
   double cpu, gpu = time_cmd(cmd, &cpu);
   char name[64];
   snprintf(name, sizeof(name), "%s %s", label, pl_name[pl]);
   report(name, "Mdraws/s", ndraws, gpu, cpu);
   vkFreeCommandBuffers(dev, cpool, 1, &cmd);
   reset_sets();
   free_buffer(&ubo);
}

/* Indexed grid mesh of n x n cells drawn ndraws times.  Vertex and index
 * buffers in vb_pl; with ubo_pl >= 0 every draw gets its own dynamic UBO
 * (matrix + color) from a buffer in ubo_pl.
 */
static void
test_mesh(VkPipeline p, int n, int ndraws, enum placement vb_pl, int ubo_pl,
          const char *label)
{
   uint32_t nv = (n + 1) * (n + 1);
   uint64_t ni = (uint64_t)n * n * 6;
   float *v = malloc(nv * 16ull);
   for (int y = 0; y <= n; y++)
      for (int x = 0; x <= n; x++) {
         float *q = v + 4 * (y * (n + 1) + x);
         q[0] = x * 2.0f / n - 1.0f;
         q[1] = y * 2.0f / n - 1.0f;
         q[2] = 0.5f + 0.25f * ((x ^ y) & 1);
         q[3] = 1.0f;
      }
   uint32_t *idx = malloc(ni * 4);
   uint64_t k = 0;
   for (int y = 0; y < n; y++)
      for (int x = 0; x < n; x++) {
         uint32_t a = y * (n + 1) + x, b = a + 1, c = a + n + 1, d = c + 1;
         idx[k++] = a; idx[k++] = b; idx[k++] = c;
         idx[k++] = b; idx[k++] = d; idx[k++] = c;
      }
   struct buf vb, ib, ubo = { 0 };
   char name[64];
   snprintf(name, sizeof(name), "%s vb %s ubo %s", label, pl_name[vb_pl],
            ubo_pl >= 0 ? pl_name[ubo_pl] : "-");
   bool ok = upload_buffer(&vb, v, nv * 16ull, VK_BUFFER_USAGE_VERTEX_BUFFER_BIT, vb_pl) &&
             upload_buffer(&ib, idx, ni * 4, VK_BUFFER_USAGE_INDEX_BUFFER_BIT, vb_pl);
   free(v);
   free(idx);
   if (ok && ubo_pl >= 0)
      ok = make_draw_ubos(&ubo, ndraws, ubo_pl, 0.3f);
   if (!ok) {
      printf("%-28s (no memory type)\n", name);
      return;
   }
   VkDescriptorSet set = set_default;
   if (ubo_pl >= 0) {
      set = alloc_set(dsl_gfx);
      write_gfx_set(set, ubo.buf);
   }

   VkCommandBuffer cmd = begin_cmd();
   ts_begin(cmd);
   begin_render(cmd, &rt8, true, true);
   vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, p);
   VkDeviceSize zero = 0;
   vkCmdBindVertexBuffers(cmd, 0, 1, &vb.buf, &zero);
   vkCmdBindIndexBuffer(cmd, ib.buf, 0, VK_INDEX_TYPE_UINT32);
   for (int i = 0; i < ndraws; i++) {
      uint32_t off = ubo_pl >= 0 ? i * UBO_STRIDE : 0;
      if (i == 0 || ubo_pl >= 0)
         vkCmdBindDescriptorSets(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, pl_gfx,
                                 0, 1, &set, 1, &off);
      vkCmdDrawIndexed(cmd, ni, 1, 0, 0, 0);
   }
   vkCmdEndRendering(cmd);
   ts_end(cmd);
   double cpu, gpu = time_cmd(cmd, &cpu);
   report(name, "Mtri/s", (double)ni / 3 * ndraws, gpu, cpu);
   vkFreeCommandBuffers(dev, cpool, 1, &cmd);
   free_buffer(&vb);
   free_buffer(&ib);
   free_buffer(&ubo);
   reset_sets();
}

static void
test_tess(VkPipeline p, float level, int grid)
{
   VkCommandBuffer cmd = begin_cmd();
   ts_begin(cmd);
   begin_render(cmd, &rt8, true, true);
   vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, p);
   uint32_t off = 0;
   vkCmdBindDescriptorSets(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, pl_gfx, 0, 1,
                           &set_default, 1, &off);
   float pc[4] = { level, (float)grid, 0, 0 };
   vkCmdPushConstants(cmd, pl_gfx, VK_SHADER_STAGE_ALL, 0, 16, pc);
   vkCmdDraw(cmd, grid * grid * 4, 1, 0, 0);
   vkCmdEndRendering(cmd);
   ts_end(cmd);
   double cpu, gpu = time_cmd(cmd, &cpu);
   char name[64];
   snprintf(name, sizeof(name), "tess L%.0f %dx%d patches", level, grid, grid);
   double tris = (double)grid * grid * level * level * 2;
   report(name, "Mtri/s", tris, gpu, cpu);
   vkFreeCommandBuffers(dev, cpool, 1, &cmd);
}

static void
test_copy(VkPipeline p, VkDeviceSize size, enum placement src_pl, int passes)
{
   struct buf a, b;
   if (!make_buffer(&a, size, VK_BUFFER_USAGE_STORAGE_BUFFER_BIT, src_pl)) {
      printf("compute copy %s: no memory type\n", pl_name[src_pl]);
      return;
   }
   make_buffer(&b, size, VK_BUFFER_USAGE_STORAGE_BUFFER_BIT, PL_DEVICE);
   VkDescriptorSet s = alloc_set(dsl_comp);
   VkDescriptorBufferInfo bi[2] = { { a.buf, 0, size }, { b.buf, 0, size } };
   VkWriteDescriptorSet w = {
      .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = s,
      .dstBinding = 0, .descriptorCount = 2,
      .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, .pBufferInfo = bi,
   };
   vkUpdateDescriptorSets(dev, 1, &w, 0, NULL);

   VkCommandBuffer cmd = begin_cmd();
   ts_begin(cmd);
   vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_COMPUTE, p);
   vkCmdBindDescriptorSets(cmd, VK_PIPELINE_BIND_POINT_COMPUTE, pl_comp, 0, 1,
                           &s, 0, NULL);
   for (int i = 0; i < passes; i++) {
      vkCmdDispatch(cmd, size / 16 / 256, 1, 1);
      barrier_all(cmd);
   }
   ts_end(cmd);
   double cpu, gpu = time_cmd(cmd, &cpu);
   char name[64];
   snprintf(name, sizeof(name), "compute copy src %s", pl_name[src_pl]);
   report(name, "GB/s", (double)size * 2 * passes, gpu, cpu);
   vkFreeCommandBuffers(dev, cpool, 1, &cmd);
   free_buffer(&a);
   free_buffer(&b);
}

static void
test_transfer_copy(VkDeviceSize size)
{
   struct buf a, b;
   make_buffer(&a, size, 0, PL_DEVICE);
   make_buffer(&b, size, 0, PL_DEVICE);
   VkCommandBuffer cmd = begin_cmd();
   ts_begin(cmd);
   VkBufferCopy r = { 0, 0, size };
   for (int i = 0; i < 4; i++) {
      vkCmdCopyBuffer(cmd, a.buf, b.buf, 1, &r);
      barrier_all(cmd);
   }
   ts_end(cmd);
   double cpu, gpu = time_cmd(cmd, &cpu);
   report("vkCmdCopyBuffer vid->vid", "GB/s", (double)size * 2 * 4, gpu, cpu);
   vkFreeCommandBuffers(dev, cpool, 1, &cmd);
   free_buffer(&a);
   free_buffer(&b);
}

static void
test_clear(void)
{
   VkCommandBuffer cmd = begin_cmd();
   ts_begin(cmd);
   VkClearColorValue cv = { .float32 = { 0.1f, 0.2f, 0.3f, 1.0f } };
   VkImageSubresourceRange rr = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 };
   for (int i = 0; i < 16; i++)
      vkCmdClearColorImage(cmd, rt16.img, VK_IMAGE_LAYOUT_GENERAL, &cv, 1, &rr);
   ts_end(cmd);
   double cpu, gpu = time_cmd(cmd, &cpu);
   report("clear rgba16f x16", "Gpix/s", (double)W * H * 16, gpu, cpu);
   vkFreeCommandBuffers(dev, cpool, 1, &cmd);
}

/* Host-visible memory coherence: the CPU writes A, the GPU computes
 * B = A * 1.0001 (both host-visible), the CPU checks B; repeated with new
 * data in the same buffers, so stale GPU cache lines would show.
 */
static void
test_coherence(VkPipeline p)
{
   const VkDeviceSize size = 16ull << 20;
   const uint32_t n = size / 4;
   struct buf a, b;
   make_buffer(&a, size, VK_BUFFER_USAGE_STORAGE_BUFFER_BIT, PL_HOST);
   make_buffer(&b, size, VK_BUFFER_USAGE_STORAGE_BUFFER_BIT, PL_HOST);
   VkDescriptorSet s = alloc_set(dsl_comp);
   VkDescriptorBufferInfo bi[2] = { { a.buf, 0, size }, { b.buf, 0, size } };
   VkWriteDescriptorSet w = {
      .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = s,
      .dstBinding = 0, .descriptorCount = 2,
      .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, .pBufferInfo = bi,
   };
   vkUpdateDescriptorSets(dev, 1, &w, 0, NULL);
   VkCommandBuffer cmd = begin_cmd();
   vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_COMPUTE, p);
   vkCmdBindDescriptorSets(cmd, VK_PIPELINE_BIND_POINT_COMPUTE, pl_comp, 0, 1,
                           &s, 0, NULL);
   vkCmdDispatch(cmd, size / 16 / 256, 1, 1);
   VkMemoryBarrier mb = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
      .srcAccessMask = VK_ACCESS_SHADER_WRITE_BIT,
      .dstAccessMask = VK_ACCESS_HOST_READ_BIT,
   };
   if (!getenv("COH_NOBARRIER"))
      vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
                           VK_PIPELINE_STAGE_HOST_BIT, 0, 1, &mb, 0, NULL, 0, NULL);
   CHECK(vkEndCommandBuffer(cmd));
   unsigned bad_total = 0;
   for (int it = 0; it < 16; it++) {
      float *fa = a.map, *fb = b.map;
      for (uint32_t i = 0; i < n; i++)
         fa[i] = (float)((i * 7 + it * 1000003u) & 0xffffff);
      submit_wait(cmd);
      unsigned bad = 0;
      for (uint32_t i = 0; i < n; i++) {
         volatile float x = fa[i];
         float e = x * 1.0001f;
         if (fb[i] != e)
            bad++;
      }
      bad_total += bad;
   }
   printf("%-28s %s (%u stale of %u)\n", "coherence host<->gpu",
          bad_total ? "FAIL" : "ok", bad_total, 16 * n);
   vkFreeCommandBuffers(dev, cpool, 1, &cmd);
   free_buffer(&a);
   free_buffer(&b);
}

static uint64_t
image_hash(struct img *im, VkImageAspectFlags aspect, VkImageLayout layout,
           uint32_t bpp)
{
   struct buf b;
   make_buffer(&b, (VkDeviceSize)W * H * bpp, 0, PL_HOST);
   VkCommandBuffer c = begin_cmd();
   if (layout != VK_IMAGE_LAYOUT_GENERAL)
      image_layout(c, im->img, aspect, layout, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL);
   VkBufferImageCopy r = {
      .imageSubresource = { aspect, 0, 0, 1 },
      .imageExtent = { W, H, 1 },
   };
   vkCmdCopyImageToBuffer(c, im->img, layout != VK_IMAGE_LAYOUT_GENERAL ?
                          VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL : layout,
                          b.buf, 1, &r);
   if (layout != VK_IMAGE_LAYOUT_GENERAL)
      image_layout(c, im->img, aspect, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, layout);
   VkMemoryBarrier mb = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
      .srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
      .dstAccessMask = VK_ACCESS_HOST_READ_BIT,
   };
   vkCmdPipelineBarrier(c, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_HOST_BIT,
                        0, 1, &mb, 0, NULL, 0, NULL);
   CHECK(vkEndCommandBuffer(c));
   submit_wait(c);
   vkFreeCommandBuffers(dev, cpool, 1, &c);
   uint64_t h = 1469598103934665603ull;
   const uint8_t *p = b.map;
   for (uint64_t i = 0; i < b.size; i++)
      h = (h ^ p[i]) * 1099511628211ull;
   free_buffer(&b);
   return h;
}

/* Renders known content through every path the perf tests use and prints
 * image hashes, to compare drivers / settings bit for bit.
 */
static void
test_verify(VkPipeline p_tex, VkPipeline p_blend, VkPipeline p_tess)
{
   float pc[4] = { 1.6f / TEX_DIM, 1.6f / TEX_DIM, 0, 0 };
   VkCommandBuffer cmd = begin_cmd();
   /* 1: textured pass into rt8 */
   begin_render(cmd, &rt8, true, false);
   vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, p_tex);
   uint32_t off = 0;
   vkCmdBindDescriptorSets(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, pl_gfx, 0, 1,
                           &set_default, 1, &off);
   vkCmdPushConstants(cmd, pl_gfx, VK_SHADER_STAGE_ALL, 0, 16, pc);
   vkCmdDraw(cmd, 3, 1, 0, 0);
   vkCmdEndRendering(cmd);
   /* 2: rt16 cleared and blended over */
   VkClearColorValue cv = { .float32 = { 0.1f, 0.2f, 0.3f, 1.0f } };
   VkImageSubresourceRange rr = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 };
   vkCmdClearColorImage(cmd, rt16.img, VK_IMAGE_LAYOUT_GENERAL, &cv, 1, &rr);
   barrier_all(cmd);
   begin_render(cmd, &rt16, false, false);
   vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, p_blend);
   vkCmdDraw(cmd, 3, 1, 0, 0);
   vkCmdDraw(cmd, 3, 1, 0, 0);
   vkCmdEndRendering(cmd);
   /* 3: tessellated, depth-tested geometry into rt8b */
   begin_render(cmd, &rt8b, true, true);
   vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, p_tess);
   vkCmdBindDescriptorSets(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, pl_gfx, 0, 1,
                           &set_default, 1, &off);
   float tpc[4] = { 16, 32, 0, 0 };
   vkCmdPushConstants(cmd, pl_gfx, VK_SHADER_STAGE_ALL, 0, 16, tpc);
   vkCmdDraw(cmd, 32 * 32 * 4, 1, 0, 0);
   vkCmdEndRendering(cmd);
   CHECK(vkEndCommandBuffer(cmd));
   submit_wait(cmd);
   vkFreeCommandBuffers(dev, cpool, 1, &cmd);

   uint64_t h1 = image_hash(&rt8, VK_IMAGE_ASPECT_COLOR_BIT,
                            VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL, 4);
   uint64_t h2 = image_hash(&rt16, VK_IMAGE_ASPECT_COLOR_BIT,
                            VK_IMAGE_LAYOUT_GENERAL, 8);
   uint64_t h3 = image_hash(&rt8b, VK_IMAGE_ASPECT_COLOR_BIT,
                            VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL, 4);
   uint64_t h4 = image_hash(&depth, VK_IMAGE_ASPECT_DEPTH_BIT,
                            VK_IMAGE_LAYOUT_DEPTH_ATTACHMENT_OPTIMAL, 4);

   /* 4: sample the (possibly compressed) rt8b into rt8 */
   VkDescriptorSet s = alloc_set(dsl_gfx);
   VkDescriptorBufferInfo bi = { dummy_ubo.buf, 0, 256 };
   VkDescriptorImageInfo ii = { sampler, rt8b.view,
                                VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL };
   VkWriteDescriptorSet w[2] = {
      { .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = s,
        .dstBinding = 0, .descriptorCount = 1,
        .descriptorType = VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER_DYNAMIC,
        .pBufferInfo = &bi },
      { .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = s,
        .dstBinding = 1, .descriptorCount = 1,
        .descriptorType = VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER,
        .pImageInfo = &ii },
   };
   vkUpdateDescriptorSets(dev, 2, w, 0, NULL);
   cmd = begin_cmd();
   image_layout(cmd, rt8b.img, VK_IMAGE_ASPECT_COLOR_BIT,
                VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
                VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL);
   begin_render(cmd, &rt8, true, false);
   vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, p_tex);
   vkCmdBindDescriptorSets(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, pl_gfx, 0, 1,
                           &s, 1, &off);
   float pc2[4] = { 1.0f / W, 1.0f / H, 0, 0 };
   vkCmdPushConstants(cmd, pl_gfx, VK_SHADER_STAGE_ALL, 0, 16, pc2);
   vkCmdDraw(cmd, 3, 1, 0, 0);
   vkCmdEndRendering(cmd);
   image_layout(cmd, rt8b.img, VK_IMAGE_ASPECT_COLOR_BIT,
                VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL,
                VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL);
   CHECK(vkEndCommandBuffer(cmd));
   submit_wait(cmd);
   vkFreeCommandBuffers(dev, cpool, 1, &cmd);
   uint64_t h5 = image_hash(&rt8, VK_IMAGE_ASPECT_COLOR_BIT,
                            VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL, 4);
   reset_sets();
   printf("verify tex %016llx blend %016llx tess %016llx depth %016llx "
          "resample %016llx\n", (unsigned long long)h1, (unsigned long long)h2,
          (unsigned long long)h3, (unsigned long long)h4, (unsigned long long)h5);
}

/* Overdraw with depth test: 32 fullscreen layers, front to back (all but
 * the first are hidden: ZCULL territory) or back to front. */
static void
test_zcull(VkPipeline p, bool front_to_back)
{
   VkCommandBuffer cmd = begin_cmd();
   ts_begin(cmd);
   begin_render(cmd, &rt8, true, true);
   vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, p);
   uint32_t off = 0;
   vkCmdBindDescriptorSets(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, pl_gfx, 0, 1,
                           &set_default, 1, &off);
   for (int i = 0; i < 32; i++) {
      int l = front_to_back ? i : 31 - i;
      float pc[4] = { 0.5f, 16, 0.1f + l * 0.025f, 0 };
      vkCmdPushConstants(cmd, pl_gfx, VK_SHADER_STAGE_ALL, 0, 16, pc);
      vkCmdDraw(cmd, 3, 1, 0, 0);
   }
   vkCmdEndRendering(cmd);
   ts_end(cmd);
   double cpu, gpu = time_cmd(cmd, &cpu);
   report(front_to_back ? "depth overdraw front-to-back" : "depth overdraw back-to-front",
          "Gpix/s", (double)W * H * 32, gpu, cpu);
   vkFreeCommandBuffers(dev, cpool, 1, &cmd);
}

/* Displacement-mapped terrain: triangle patches, distance-based levels
 * from a UBO in the TCS, three texture fetches in the TES; ndraws draws
 * of grid x grid cells with their own dynamic UBO each. */
static void
test_tess_terrain(VkPipeline p, float level, int grid, int ndraws)
{
   struct buf ubo;
   make_draw_ubos(&ubo, ndraws, PL_DEVICE, 1.0f);
   float *m = calloc(ndraws, UBO_STRIDE);
   for (int i = 0; i < ndraws; i++) {
      float *q = m + i * UBO_STRIDE / 4;
      q[0] = q[5] = q[10] = q[15] = 1.0f;
      q[12] = (i % 4) * 0.05f; q[13] = (i / 4 % 4) * 0.05f;
      q[16] = q[17] = q[18] = q[19] = 1.0f;
   }
   free_buffer(&ubo);
   upload_buffer(&ubo, m, (VkDeviceSize)ndraws * UBO_STRIDE,
                 VK_BUFFER_USAGE_UNIFORM_BUFFER_BIT, PL_DEVICE);
   free(m);
   VkDescriptorSet set = alloc_set(dsl_gfx);
   write_gfx_set(set, ubo.buf);
   VkCommandBuffer cmd = begin_cmd();
   ts_begin(cmd);
   begin_render(cmd, &rt8, true, true);
   vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, p);
   float pc[4] = { level, (float)grid, 0, 0 };
   vkCmdPushConstants(cmd, pl_gfx, VK_SHADER_STAGE_ALL, 0, 16, pc);
   for (int i = 0; i < ndraws; i++) {
      uint32_t off = i * UBO_STRIDE;
      vkCmdBindDescriptorSets(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, pl_gfx, 0,
                              1, &set, 1, &off);
      vkCmdDraw(cmd, grid * grid * 6, 1, 0, 0);
   }
   vkCmdEndRendering(cmd);
   ts_end(cmd);
   double cpu, gpu = time_cmd(cmd, &cpu);
   char name[64];
   snprintf(name, sizeof(name), "terrain L%.0f %dx%d x%d", level, grid, grid, ndraws);
   report(name, "Mpatch/s", (double)grid * grid * 2 * ndraws, gpu, cpu);
   vkFreeCommandBuffers(dev, cpool, 1, &cmd);
   free_buffer(&ubo);
   reset_sets();
}

static bool
want(const char *tests, const char *t)
{
   if (tests == NULL)
      return true;
   const char *p = strstr(tests, t);
   return p != NULL;
}

int
main(int argc, char **argv)
{
   const char *tests = NULL;
   for (int i = 1; i < argc; i++) {
      if (!strcmp(argv[i], "-r") && i + 1 < argc)
         reps = atoi(argv[++i]);
      else if (!strcmp(argv[i], "-t") && i + 1 < argc)
         tests = argv[++i];
   }

   VkApplicationInfo app = {
      .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .pApplicationName = "vk_perf_bench",
      .apiVersion = VK_API_VERSION_1_3,
   };
   VkInstanceCreateInfo ici = {
      .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
      .pApplicationInfo = &app,
   };
   CHECK(vkCreateInstance(&ici, NULL, &inst));
   uint32_t n = 8;
   VkPhysicalDevice pdevs[8];
   CHECK(vkEnumeratePhysicalDevices(inst, &n, pdevs));
   pdev = VK_NULL_HANDLE;
   for (uint32_t i = 0; i < n; i++) {
      vkGetPhysicalDeviceProperties(pdevs[i], &props);
      if (props.deviceType == VK_PHYSICAL_DEVICE_TYPE_DISCRETE_GPU) {
         pdev = pdevs[i];
         break;
      }
   }
   if (pdev == VK_NULL_HANDLE) {
      fprintf(stderr, "no discrete GPU\n");
      return 1;
   }
   vkGetPhysicalDeviceProperties(pdev, &props);
   vkGetPhysicalDeviceMemoryProperties(pdev, &memprops);
   VkPhysicalDeviceDriverProperties drv = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRIVER_PROPERTIES,
   };
   VkPhysicalDeviceProperties2 p2 = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2, .pNext = &drv,
   };
   vkGetPhysicalDeviceProperties2(pdev, &p2);
   printf("# %s / %s %s, %ux%u\n", props.deviceName, drv.driverName,
          drv.driverInfo, W, H);
   for (uint32_t i = 0; i < memprops.memoryTypeCount; i++)
      printf("#  memtype %u heap %u flags 0x%x\n", i,
             memprops.memoryTypes[i].heapIndex,
             memprops.memoryTypes[i].propertyFlags);

   uint32_t nq = 8;
   VkQueueFamilyProperties qf[8];
   vkGetPhysicalDeviceQueueFamilyProperties(pdev, &nq, qf);
   for (qfam = 0; qfam < nq; qfam++)
      if (qf[qfam].queueFlags & VK_QUEUE_GRAPHICS_BIT)
         break;
   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = qfam, .queueCount = 1, .pQueuePriorities = &prio,
   };
   VkPhysicalDeviceVulkan13Features f13 = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES,
      .dynamicRendering = VK_TRUE,
   };
   VkPhysicalDeviceFeatures2 f2 = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2, .pNext = &f13,
      .features.tessellationShader = VK_TRUE,
   };
   VkDeviceCreateInfo dci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
      .pNext = &f2,
      .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci,
   };
   CHECK(vkCreateDevice(pdev, &dci, NULL, &dev));
   vkGetDeviceQueue(dev, qfam, 0, &queue);

   VkCommandPoolCreateInfo cpi = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
      .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
      .queueFamilyIndex = qfam,
   };
   CHECK(vkCreateCommandPool(dev, &cpi, NULL, &cpool));
   VkQueryPoolCreateInfo qpi = {
      .sType = VK_STRUCTURE_TYPE_QUERY_POOL_CREATE_INFO,
      .queryType = VK_QUERY_TYPE_TIMESTAMP, .queryCount = 2,
   };
   CHECK(vkCreateQueryPool(dev, &qpi, NULL, &qpool));

   /* layouts */
   VkDescriptorSetLayoutBinding gb[2] = {
      { 0, VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER_DYNAMIC, 1, VK_SHADER_STAGE_ALL, NULL },
      { 1, VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, 1, VK_SHADER_STAGE_ALL, NULL },
   };
   VkDescriptorSetLayoutCreateInfo dli = {
      .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
      .bindingCount = 2, .pBindings = gb,
   };
   CHECK(vkCreateDescriptorSetLayout(dev, &dli, NULL, &dsl_gfx));
   VkDescriptorSetLayoutBinding cbnd[2] = {
      { 0, VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 1, VK_SHADER_STAGE_COMPUTE_BIT, NULL },
      { 1, VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 1, VK_SHADER_STAGE_COMPUTE_BIT, NULL },
   };
   dli.pBindings = cbnd;
   CHECK(vkCreateDescriptorSetLayout(dev, &dli, NULL, &dsl_comp));
   VkPushConstantRange pcr = { VK_SHADER_STAGE_ALL, 0, 16 };
   VkPipelineLayoutCreateInfo pli = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
      .setLayoutCount = 1, .pSetLayouts = &dsl_gfx,
      .pushConstantRangeCount = 1, .pPushConstantRanges = &pcr,
   };
   CHECK(vkCreatePipelineLayout(dev, &pli, NULL, &pl_gfx));
   pli.pSetLayouts = &dsl_comp;
   pli.pushConstantRangeCount = 0;
   CHECK(vkCreatePipelineLayout(dev, &pli, NULL, &pl_comp));
   VkDescriptorPoolSize ps[3] = {
      { VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER_DYNAMIC, 256 },
      { VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, 256 },
      { VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 64 },
   };
   VkDescriptorPoolCreateInfo dpi = {
      .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
      .maxSets = 256, .poolSizeCount = 3, .pPoolSizes = ps,
   };
   CHECK(vkCreateDescriptorPool(dev, &dpi, NULL, &dpool));

   /* images */
   const VkImageUsageFlags rt_usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT |
      VK_IMAGE_USAGE_TRANSFER_DST_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT |
      VK_IMAGE_USAGE_SAMPLED_BIT;
   make_image(&rt8, VK_FORMAT_R8G8B8A8_UNORM, W, H, rt_usage,
              VK_IMAGE_ASPECT_COLOR_BIT);
   make_image(&rt8b, VK_FORMAT_R8G8B8A8_UNORM, W, H, rt_usage,
              VK_IMAGE_ASPECT_COLOR_BIT);
   make_image(&rt16, VK_FORMAT_R16G16B16A16_SFLOAT, W, H, rt_usage,
              VK_IMAGE_ASPECT_COLOR_BIT);
   make_image(&depth, VK_FORMAT_D32_SFLOAT, W, H,
              VK_IMAGE_USAGE_DEPTH_STENCIL_ATTACHMENT_BIT |
              VK_IMAGE_USAGE_TRANSFER_SRC_BIT, VK_IMAGE_ASPECT_DEPTH_BIT);
   make_image(&tex, VK_FORMAT_R8G8B8A8_UNORM, TEX_DIM, TEX_DIM,
              VK_IMAGE_USAGE_SAMPLED_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT,
              VK_IMAGE_ASPECT_COLOR_BIT);
   {
      struct buf st;
      make_buffer(&st, (VkDeviceSize)TEX_DIM * TEX_DIM * 4, 0, PL_HOST);
      uint32_t *px = st.map;
      uint32_t s = 12345;
      for (uint64_t i = 0; i < (uint64_t)TEX_DIM * TEX_DIM; i++) {
         s = s * 1664525u + 1013904223u;
         px[i] = s;
      }
      VkCommandBuffer c = begin_cmd();
      image_layout(c, rt8.img, VK_IMAGE_ASPECT_COLOR_BIT, VK_IMAGE_LAYOUT_UNDEFINED,
                   VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL);
      image_layout(c, rt8b.img, VK_IMAGE_ASPECT_COLOR_BIT, VK_IMAGE_LAYOUT_UNDEFINED,
                   VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL);
      image_layout(c, rt16.img, VK_IMAGE_ASPECT_COLOR_BIT, VK_IMAGE_LAYOUT_UNDEFINED,
                   VK_IMAGE_LAYOUT_GENERAL);
      image_layout(c, depth.img, VK_IMAGE_ASPECT_DEPTH_BIT, VK_IMAGE_LAYOUT_UNDEFINED,
                   VK_IMAGE_LAYOUT_DEPTH_ATTACHMENT_OPTIMAL);
      image_layout(c, tex.img, VK_IMAGE_ASPECT_COLOR_BIT, VK_IMAGE_LAYOUT_UNDEFINED,
                   VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL);
      VkBufferImageCopy r = {
         .imageSubresource = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1 },
         .imageExtent = { TEX_DIM, TEX_DIM, 1 },
      };
      vkCmdCopyBufferToImage(c, st.buf, tex.img,
                             VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, 1, &r);
      image_layout(c, tex.img, VK_IMAGE_ASPECT_COLOR_BIT,
                   VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                   VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL);
      CHECK(vkEndCommandBuffer(c));
      submit_wait(c);
      vkFreeCommandBuffers(dev, cpool, 1, &c);
      free_buffer(&st);
   }
   VkSamplerCreateInfo sci = {
      .sType = VK_STRUCTURE_TYPE_SAMPLER_CREATE_INFO,
      .magFilter = VK_FILTER_LINEAR, .minFilter = VK_FILTER_LINEAR,
      .addressModeU = VK_SAMPLER_ADDRESS_MODE_REPEAT,
      .addressModeV = VK_SAMPLER_ADDRESS_MODE_REPEAT,
      .addressModeW = VK_SAMPLER_ADDRESS_MODE_REPEAT,
      .maxLod = 0.0f,
   };
   CHECK(vkCreateSampler(dev, &sci, NULL, &sampler));
   make_buffer(&dummy_ubo, 4096, VK_BUFFER_USAGE_UNIFORM_BUFFER_BIT, PL_DEVICE);
   set_default = alloc_set(dsl_gfx);
   write_gfx_set(set_default, dummy_ubo.buf);

   m_fsq = SHADER(spv_fsq_vert);
   m_trivs = SHADER(spv_tri_vert);
   m_tritcs = SHADER(spv_tri_tesc);
   m_trites = SHADER(spv_tri_tese);
   m_fsqz = SHADER(spv_fsqz_vert);
   m_alu = SHADER(spv_alu_frag);
   m_tex = SHADER(spv_tex_frag);
   m_blend = SHADER(spv_blend_frag);
   m_ubo = SHADER(spv_ubo_vert);
   m_color = SHADER(spv_color_frag);
   m_colortex = SHADER(spv_colortex_frag);
   m_mesh = SHADER(spv_mesh_vert);
   m_meshubo = SHADER(spv_meshubo_vert);
   m_tvs = SHADER(spv_tess_vert);
   m_tcs = SHADER(spv_tess_tesc);
   m_tes = SHADER(spv_tess_tese);
   m_copy = SHADER(spv_copy_comp);

   const VkPrimitiveTopology TL = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST;
   VkPipeline p_alu = make_gfx(&(struct gfx_desc){ .vs = m_fsq, .fs = m_alu,
      .color = VK_FORMAT_R8G8B8A8_UNORM, .topo = TL });
   VkPipeline p_aluz = make_gfx(&(struct gfx_desc){ .vs = m_fsqz, .fs = m_alu,
      .color = VK_FORMAT_R8G8B8A8_UNORM, .depth = true, .topo = TL });
   VkPipeline p_tess2 = make_gfx(&(struct gfx_desc){ .vs = m_trivs, .tcs = m_tritcs,
      .tes = m_trites, .fs = m_color, .color = VK_FORMAT_R8G8B8A8_UNORM,
      .depth = true, .topo = VK_PRIMITIVE_TOPOLOGY_PATCH_LIST, .patch3 = true });
   VkPipeline p_tex = make_gfx(&(struct gfx_desc){ .vs = m_fsq, .fs = m_tex,
      .color = VK_FORMAT_R8G8B8A8_UNORM, .topo = TL });
   VkPipeline p_blend = make_gfx(&(struct gfx_desc){ .vs = m_fsq, .fs = m_blend,
      .color = VK_FORMAT_R16G16B16A16_SFLOAT, .blend = true, .topo = TL });
   VkPipeline p_fill8 = make_gfx(&(struct gfx_desc){ .vs = m_fsq, .fs = m_blend,
      .color = VK_FORMAT_R8G8B8A8_UNORM, .topo = TL });
   VkPipeline p_ubo = make_gfx(&(struct gfx_desc){ .vs = m_ubo, .fs = m_color,
      .color = VK_FORMAT_R8G8B8A8_UNORM,
      .topo = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_STRIP });
   VkPipeline p_ubotex = make_gfx(&(struct gfx_desc){ .vs = m_ubo, .fs = m_colortex,
      .color = VK_FORMAT_R8G8B8A8_UNORM,
      .topo = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_STRIP });
   VkPipeline p_mesh = make_gfx(&(struct gfx_desc){ .vs = m_mesh, .fs = m_color,
      .color = VK_FORMAT_R8G8B8A8_UNORM, .depth = true, .mesh_vb = true, .topo = TL });
   VkPipeline p_meshubo = make_gfx(&(struct gfx_desc){ .vs = m_meshubo, .fs = m_color,
      .color = VK_FORMAT_R8G8B8A8_UNORM, .depth = true, .mesh_vb = true, .topo = TL });
   VkPipeline p_tess = make_gfx(&(struct gfx_desc){ .vs = m_tvs, .tcs = m_tcs,
      .tes = m_tes, .fs = m_color, .color = VK_FORMAT_R8G8B8A8_UNORM,
      .depth = true, .topo = VK_PRIMITIVE_TOPOLOGY_PATCH_LIST });
   VkComputePipelineCreateInfo cpci = {
      .sType = VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO,
      .stage = { .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
                 .stage = VK_SHADER_STAGE_COMPUTE_BIT, .module = m_copy,
                 .pName = "main" },
      .layout = pl_comp,
   };
   VkPipeline p_copy;
   CHECK(vkCreateComputePipelines(dev, VK_NULL_HANDLE, 1, &cpci, NULL, &p_copy));

   const double px = (double)W * H;
   if (want(tests, "alu")) {
      float pc[4] = { 0.5f, 64, 0, 0 };
      test_fullscreen("alu frag 64 iter x4", p_alu, &rt8, 4, pc, "Gpix/s", px * 4);
   }
   if (want(tests, "tex")) {
      float pc[4] = { 1.6f / TEX_DIM, 1.6f / TEX_DIM, 0, 0 };
      test_fullscreen("tex 8 taps 4k rgba8 x4", p_tex, &rt8, 4, pc, "Gtap/s", px * 32);
      float pc2[4] = { 1.0f / TEX_DIM / 4, 1.0f / TEX_DIM / 4, 0, 0 };
      test_fullscreen("tex 8 taps cached x4", p_tex, &rt8, 4, pc2, "Gtap/s", px * 32);
   }
   if (want(tests, "fill")) {
      float pc[4] = { 0 };
      test_fullscreen("fill rgba8 x16", p_fill8, &rt8, 16, pc, "Gpix/s", px * 16);
      test_fullscreen("blend rgba16f x16", p_blend, &rt16, 16, pc, "Gpix/s", px * 16);
   }
   if (want(tests, "zcull")) {
      test_zcull(p_aluz, true);
      test_zcull(p_aluz, false);
   }
   if (want(tests, "clear"))
      test_clear();
   if (want(tests, "ubo")) {
      test_ubo_draws(p_ubo, PL_DEVICE, 20000, false, "plain draws");
      test_ubo_draws(p_ubo, PL_DEVICE, 20000, false, "ubo draws");
      test_ubo_draws(p_ubo, PL_HOST, 20000, false, "ubo draws");
      test_ubo_draws(p_ubo, PL_DEVICE_HOST, 20000, false, "ubo draws");
   }
   if (want(tests, "desc")) {
      test_ubo_draws(p_ubotex, PL_DEVICE, 20000, true, "set switch");
   }
   if (want(tests, "mesh")) {
      test_mesh(p_mesh, 1024, 4, PL_DEVICE, -1, "mesh 2M");
      test_mesh(p_mesh, 1024, 4, PL_HOST, -1, "mesh 2M");
   }
   if (want(tests, "dyn")) {
      for (int vb = PL_DEVICE; vb <= PL_HOST; vb++)
         for (int u = PL_DEVICE; u <= PL_DEVICE_HOST; u++)
            test_mesh(p_meshubo, 32, 2000, vb, u, "dyn 2k x 2k");
   }
   if (want(tests, "tess")) {
      test_tess(p_tess, 16, 64);
      test_tess(p_tess, 64, 16);
   }
   if (want(tests, "terrain")) {
      test_tess_terrain(p_tess2, 4, 128, 4);
      test_tess_terrain(p_tess2, 16, 64, 4);
      test_tess_terrain(p_tess2, 16, 8, 500);
   }
   if (want(tests, "copy")) {
      test_copy(p_copy, 256ull << 20, PL_DEVICE, 4);
      test_copy(p_copy, 64ull << 20, PL_HOST, 1);
      test_transfer_copy(256ull << 20);
   }

   if (want(tests, "verify"))
      test_verify(p_tex, p_blend, p_tess);
   if (want(tests, "coh"))
      test_coherence(p_copy);

   CHECK(vkDeviceWaitIdle(dev));
   printf("# done\n");
   return 0;
}
