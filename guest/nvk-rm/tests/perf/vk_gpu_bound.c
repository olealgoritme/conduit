/*
 * vk_gpu_bound: GPU-bound throughput per stage, in GPU time from timestamp
 * queries, to compare the same GPU under different drivers and paths:
 * NVIDIA's driver on the host (the bare-metal reference), NVK on RM on the
 * host, NVK on RM in a Windows guest.  guest/windows/tools/d3d12_gpu_bound.cpp
 * runs the same tests through D3D12 (vkd3d-proton and the D3D12 UMD), so the
 * two outputs line up test by test.
 *
 *   vk_gpu_bound [-r reps] [-t test,...] [-w width -h height] [-s seconds]
 *
 * Linux:   VK_DRIVER_FILES=<icd.json> ./vk_gpu_bound
 * Windows: set NVK_RM=1 & set VK_DIRECT_DRIVER=C:\...\vulkan_nouveau.dll
 *          vk_gpu_bound.exe        (see ../vk_direct_driver.h)
 * BENCH_DEVICE=<substring> picks a device by name (default: the first
 * discrete GPU).  Offscreen only.
 *
 * Tests (default: all, in this order; sustain runs last):
 *   alu      transcendental + FMA loop, fullscreen x4             Gpix/s
 *   fma      4 independent vec4 FMA chains, fullscreen x4         TFLOP/s
 *   fill     16 fullscreen passes, RGBA8 constant / RGBA8 per-pixel noise /
 *            RGBA16F blended; each into a dedicated image (NVK on RM
 *            compresses it) and into one placed in a shared allocation, as
 *            vkd3d-proton places D3D12 resources (not compressed unless
 *            NVK_RM_COMPRESS_ALL)                                  Gpix/s
 *   tex      8 bilinear taps per pixel: 4096^2 RGBA8 texture, cached and
 *            streaming; 8192^2 (256 MiB) read at a random place per 8x8 block
 *            (page/TLB bound)                                     Gtap/s
 *   bw       compute copy of 256 MiB                              GB/s
 *   barrier  256 dependent small compute dispatches with a compute->compute
 *            barrier, an ALL_COMMANDS barrier or none between them: the cost
 *            of one barrier (NVK: a WAIT_FOR_IDLE)                us/barrier
 *   rtpass   256 960x540 draws, each reading the previous one's target:
 *            render pass + layout transition between draws, against the
 *            same draws in one pass                               us/pass
 *   record   CPU cost of recording 8 command buffers x 64 dispatches per
 *            frame: pools reused, then a new pool per command buffer
 *   submit   the same GPU work per frame (64 units of ~50 us) split into
 *            1/4/16/64 submits, each signalling a timeline semaphore, 2
 *            frames in flight: wall time vs GPU busy, the idle gap per
 *            submit (what per-submit overhead costs a GPU-bound frame)
 *   sustain  ~4 ms ALU submits back to back for -s seconds (default 10):
 *            GPU time per submit over time.  Run next to nvidia-smi dmon on
 *            the host for clocks and power; an inflated p99/max against the
 *            minimum is time the context was switched out (timeslices)
 *
 * Every test prints one line per case: "name  gpu X ms  rate unit".
 */
#include <vulkan/vulkan.h>

#include <math.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#ifdef _WIN32
#include "../vk_direct_driver.h"
#else
#define direct_driver_chain(ici) ((void)(ici))
#endif

#include "shaders_gb/alu_frag.h"
#include "shaders_gb/blit_frag.h"
#include "shaders_gb/color_frag.h"
#include "shaders_gb/copy_comp.h"
#include "shaders_gb/fma_frag.h"
#include "shaders_gb/fsq_vert.h"
#include "shaders_gb/noise_frag.h"
#include "shaders_gb/spin_comp.h"
#include "shaders_gb/tex_frag.h"
#include "shaders_gb/texrand_frag.h"

#define CHECK(x)                                                              \
   do {                                                                       \
      VkResult r_ = (x);                                                      \
      if (r_ != VK_SUCCESS) {                                                 \
         fprintf(stderr, "%s:%d: %s = %d\n", __FILE__, __LINE__, #x, r_);     \
         exit(1);                                                             \
      }                                                                       \
   } while (0)

#define MIN2(a, b) ((a) < (b) ? (a) : (b))
#define MAX2(a, b) ((a) > (b) ? (a) : (b))

static uint32_t W = 3840, H = 2160;
static int reps = 11;
static double sustain_s = 10.0;

static VkInstance inst;
static VkPhysicalDevice pdev;
static VkDevice dev;
static VkQueue queue;
static uint32_t qfam;
static VkPhysicalDeviceMemoryProperties memprops;
static VkPhysicalDeviceProperties props;
static VkCommandPool cpool;
static VkQueryPool qpool;
#define NQUERIES 1024

static double
now_ms(void)
{
#ifdef _WIN32
   static LARGE_INTEGER f;
   LARGE_INTEGER c;
   if (!f.QuadPart)
      QueryPerformanceFrequency(&f);
   QueryPerformanceCounter(&c);
   return (double)c.QuadPart * 1e3 / (double)f.QuadPart;
#else
   struct timespec ts;
   clock_gettime(CLOCK_MONOTONIC, &ts);
   return ts.tv_sec * 1e3 + ts.tv_nsec / 1e6;
#endif
}

static int
cmp_double(const void *a, const void *b)
{
   double x = *(const double *)a, y = *(const double *)b;
   return x < y ? -1 : x > y;
}

static double
pct(double *v, int n, double p)
{
   qsort(v, n, sizeof(double), cmp_double);
   int i = (int)(p * (n - 1) + 0.5);
   return v[MIN2(MAX2(i, 0), n - 1)];
}

/* ------------------------------------------------------------------ */
/* memory, buffers, images */

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
device_type(uint32_t bits)
{
   int t = find_memtype(bits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT,
                        VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT);
   if (t < 0)
      t = find_memtype(bits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT, 0);
   if (t < 0) {
      fprintf(stderr, "no device-local memory type\n");
      exit(1);
   }
   return t;
}

struct buf {
   VkBuffer buf;
   VkDeviceMemory mem;
   VkDeviceSize size;
};

static void
make_buffer(struct buf *b, VkDeviceSize size)
{
   memset(b, 0, sizeof(*b));
   b->size = size;
   VkBufferCreateInfo bci = {
      .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
      .size = size,
      .usage = VK_BUFFER_USAGE_STORAGE_BUFFER_BIT |
               VK_BUFFER_USAGE_TRANSFER_DST_BIT |
               VK_BUFFER_USAGE_TRANSFER_SRC_BIT,
   };
   CHECK(vkCreateBuffer(dev, &bci, NULL, &b->buf));
   VkMemoryRequirements req;
   vkGetBufferMemoryRequirements(dev, b->buf, &req);
   VkMemoryAllocateInfo mai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .allocationSize = req.size,
      .memoryTypeIndex = device_type(req.memoryTypeBits),
   };
   CHECK(vkAllocateMemory(dev, &mai, NULL, &b->mem));
   CHECK(vkBindBufferMemory(dev, b->buf, b->mem, 0));
}

/* One shared allocation that "placed" images are bound into, as
 * vkd3d-proton binds placed resources and committed resources it
 * sub-allocates. */
static struct {
   VkDeviceMemory mem;
   VkDeviceSize size, off;
   uint32_t type;
} heap;

struct img {
   VkImage img;
   VkDeviceMemory mem; /* VK_NULL_HANDLE when placed in heap */
   VkImageView view;
   VkFormat fmt;
   uint32_t w, h;
};

static void
make_image(struct img *im, VkFormat fmt, uint32_t w, uint32_t h,
           VkImageUsageFlags usage, bool dedicated)
{
   memset(im, 0, sizeof(*im));
   im->fmt = fmt;
   im->w = w;
   im->h = h;
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
   if (dedicated) {
      VkMemoryDedicatedAllocateInfo ded = {
         .sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
         .image = im->img,
      };
      VkMemoryAllocateInfo mai = {
         .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
         .pNext = &ded,
         .allocationSize = req.size,
         .memoryTypeIndex = device_type(req.memoryTypeBits),
      };
      CHECK(vkAllocateMemory(dev, &mai, NULL, &im->mem));
      CHECK(vkBindImageMemory(dev, im->img, im->mem, 0));
   } else {
      if (heap.mem == VK_NULL_HANDLE) {
         heap.size = 1024ull << 20;
         heap.type = device_type(req.memoryTypeBits);
         VkMemoryAllocateInfo mai = {
            .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
            .allocationSize = heap.size,
            .memoryTypeIndex = heap.type,
         };
         CHECK(vkAllocateMemory(dev, &mai, NULL, &heap.mem));
      }
      if (!(req.memoryTypeBits & (1u << heap.type))) {
         fprintf(stderr, "image cannot go in the shared allocation's type\n");
         exit(1);
      }
      VkDeviceSize align = MAX2(req.alignment, 65536);
      VkDeviceSize off = (heap.off + align - 1) & ~(align - 1);
      if (off + req.size > heap.size) {
         fprintf(stderr, "shared allocation full\n");
         exit(1);
      }
      CHECK(vkBindImageMemory(dev, im->img, heap.mem, off));
      heap.off = off + req.size;
   }
   VkImageViewCreateInfo vci = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
      .image = im->img,
      .viewType = VK_IMAGE_VIEW_TYPE_2D,
      .format = fmt,
      .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 },
   };
   CHECK(vkCreateImageView(dev, &vci, NULL, &im->view));
}

static void
free_image(struct img *im)
{
   vkDestroyImageView(dev, im->view, NULL);
   vkDestroyImage(dev, im->img, NULL);
   if (im->mem)
      vkFreeMemory(dev, im->mem, NULL);
   memset(im, 0, sizeof(*im));
}

/* Placed images live until the heap is reset (all of them freed first) */
static void
reset_heap(void)
{
   heap.off = 0;
}

/* ------------------------------------------------------------------ */
/* pipelines and descriptors */

static VkDescriptorSetLayout dsl;
static VkPipelineLayout pl;
static VkDescriptorPool dpool;
static VkSampler sampler;

struct pc {
   float p[4];
   uint32_t u[4];
};

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

static VkPipeline
make_gfx(VkShaderModule fs, VkFormat color, bool blend)
{
   static VkShaderModule vs;
   if (!vs)
      vs = SHADER(spv_fsq_vert);
   VkPipelineShaderStageCreateInfo st[2] = {
      { .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
        .stage = VK_SHADER_STAGE_VERTEX_BIT, .module = vs, .pName = "main" },
      { .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
        .stage = VK_SHADER_STAGE_FRAGMENT_BIT, .module = fs, .pName = "main" },
   };
   VkPipelineVertexInputStateCreateInfo vi = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO,
   };
   VkPipelineInputAssemblyStateCreateInfo ia = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
      .topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST,
   };
   VkPipelineViewportStateCreateInfo vps = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO,
      .viewportCount = 1, .scissorCount = 1,
   };
   VkDynamicState dyn[2] = { VK_DYNAMIC_STATE_VIEWPORT, VK_DYNAMIC_STATE_SCISSOR };
   VkPipelineDynamicStateCreateInfo dys = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_DYNAMIC_STATE_CREATE_INFO,
      .dynamicStateCount = 2, .pDynamicStates = dyn,
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
   };
   VkPipelineColorBlendAttachmentState cba = {
      .blendEnable = blend,
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
      .pColorAttachmentFormats = &color,
   };
   VkGraphicsPipelineCreateInfo ci = {
      .sType = VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO,
      .pNext = &ri,
      .stageCount = 2, .pStages = st,
      .pVertexInputState = &vi,
      .pInputAssemblyState = &ia,
      .pViewportState = &vps,
      .pRasterizationState = &rs,
      .pMultisampleState = &ms,
      .pDepthStencilState = &ds,
      .pColorBlendState = &cb,
      .pDynamicState = &dys,
      .layout = pl,
   };
   VkPipeline p;
   CHECK(vkCreateGraphicsPipelines(dev, VK_NULL_HANDLE, 1, &ci, NULL, &p));
   return p;
}

static VkPipeline
make_comp(VkShaderModule cs)
{
   VkComputePipelineCreateInfo ci = {
      .sType = VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO,
      .stage = { .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
                 .stage = VK_SHADER_STAGE_COMPUTE_BIT, .module = cs,
                 .pName = "main" },
      .layout = pl,
   };
   VkPipeline p;
   CHECK(vkCreateComputePipelines(dev, VK_NULL_HANDLE, 1, &ci, NULL, &p));
   return p;
}

/* A set with a sampled image (may be NULL) and two storage buffers */
static VkDescriptorSet
make_set(const struct img *tex, const struct buf *src, const struct buf *dst)
{
   VkDescriptorSetAllocateInfo ai = {
      .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
      .descriptorPool = dpool,
      .descriptorSetCount = 1,
      .pSetLayouts = &dsl,
   };
   VkDescriptorSet s;
   CHECK(vkAllocateDescriptorSets(dev, &ai, &s));
   VkWriteDescriptorSet w[3];
   uint32_t n = 0;
   VkDescriptorImageInfo ii;
   VkDescriptorBufferInfo bi[2];
   if (tex) {
      ii = (VkDescriptorImageInfo){ sampler, tex->view,
                                    VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL };
      w[n++] = (VkWriteDescriptorSet){
         .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = s,
         .dstBinding = 0, .descriptorCount = 1,
         .descriptorType = VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER,
         .pImageInfo = &ii };
   }
   if (src) {
      bi[0] = (VkDescriptorBufferInfo){ src->buf, 0, VK_WHOLE_SIZE };
      w[n++] = (VkWriteDescriptorSet){
         .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = s,
         .dstBinding = 1, .descriptorCount = 1,
         .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
         .pBufferInfo = &bi[0] };
   }
   if (dst) {
      bi[1] = (VkDescriptorBufferInfo){ dst->buf, 0, VK_WHOLE_SIZE };
      w[n++] = (VkWriteDescriptorSet){
         .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = s,
         .dstBinding = 2, .descriptorCount = 1,
         .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
         .pBufferInfo = &bi[1] };
   }
   vkUpdateDescriptorSets(dev, n, w, 0, NULL);
   return s;
}

/* ------------------------------------------------------------------ */
/* command buffers and timing */

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
ts_write(VkCommandBuffer cmd, uint32_t q, bool end)
{
   vkCmdWriteTimestamp2(cmd, end ? VK_PIPELINE_STAGE_2_BOTTOM_OF_PIPE_BIT
                                 : VK_PIPELINE_STAGE_2_TOP_OF_PIPE_BIT, qpool, q);
}

static double
ts_ms(uint64_t a, uint64_t b)
{
   return (double)(b - a) * props.limits.timestampPeriod / 1e6;
}

/* Records body() between timestamps 0 and 1, submits it reps times after a
 * warm-up and returns the median GPU time. */
typedef void (*body_fn)(VkCommandBuffer cmd, void *ctx);

static double
time_body(body_fn body, void *ctx)
{
   VkCommandBuffer cmd = begin_cmd();
   vkCmdResetQueryPool(cmd, qpool, 0, 2);
   ts_write(cmd, 0, false);
   body(cmd, ctx);
   ts_write(cmd, 1, true);
   CHECK(vkEndCommandBuffer(cmd));
   double gpu[64];
   int n = MIN2(reps, 64);
   submit_wait(cmd);
   for (int i = 0; i < n; i++) {
      submit_wait(cmd);
      uint64_t ts[2];
      CHECK(vkGetQueryPoolResults(dev, qpool, 0, 2, sizeof(ts), ts, 8,
                                  VK_QUERY_RESULT_64_BIT |
                                  VK_QUERY_RESULT_WAIT_BIT));
      gpu[i] = ts_ms(ts[0], ts[1]);
   }
   vkFreeCommandBuffers(dev, cpool, 1, &cmd);
   return pct(gpu, n, 0.5);
}

static void
report(const char *name, double gpu_ms, double work, const char *unit)
{
   if (work > 0)
      printf("%-34s gpu %9.3f ms  %10.2f %s\n", name, gpu_ms,
             work / (gpu_ms * 1e-3), unit);
   else
      printf("%-34s gpu %9.3f ms\n", name, gpu_ms);
   fflush(stdout);
}

static void
image_barrier(VkCommandBuffer cmd, VkImage img,
              VkPipelineStageFlags2 src_stage, VkAccessFlags2 src_access,
              VkPipelineStageFlags2 dst_stage, VkAccessFlags2 dst_access,
              VkImageLayout from, VkImageLayout to)
{
   VkImageMemoryBarrier2 b = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER_2,
      .srcStageMask = src_stage, .srcAccessMask = src_access,
      .dstStageMask = dst_stage, .dstAccessMask = dst_access,
      .oldLayout = from, .newLayout = to,
      .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
      .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
      .image = img,
      .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 },
   };
   VkDependencyInfo di = {
      .sType = VK_STRUCTURE_TYPE_DEPENDENCY_INFO,
      .imageMemoryBarrierCount = 1, .pImageMemoryBarriers = &b,
   };
   vkCmdPipelineBarrier2(cmd, &di);
}

static void
memory_barrier(VkCommandBuffer cmd,
               VkPipelineStageFlags2 src_stage, VkAccessFlags2 src_access,
               VkPipelineStageFlags2 dst_stage, VkAccessFlags2 dst_access)
{
   VkMemoryBarrier2 b = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER_2,
      .srcStageMask = src_stage, .srcAccessMask = src_access,
      .dstStageMask = dst_stage, .dstAccessMask = dst_access,
   };
   VkDependencyInfo di = {
      .sType = VK_STRUCTURE_TYPE_DEPENDENCY_INFO,
      .memoryBarrierCount = 1, .pMemoryBarriers = &b,
   };
   vkCmdPipelineBarrier2(cmd, &di);
}

/* Undefined -> layout, once, outside any timing */
static void
init_layout(struct img *im, VkImageLayout to, bool clear)
{
   VkCommandBuffer cmd = begin_cmd();
   image_barrier(cmd, im->img, VK_PIPELINE_STAGE_2_NONE, 0,
                 VK_PIPELINE_STAGE_2_ALL_COMMANDS_BIT,
                 VK_ACCESS_2_MEMORY_READ_BIT | VK_ACCESS_2_MEMORY_WRITE_BIT,
                 VK_IMAGE_LAYOUT_UNDEFINED,
                 clear ? VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL : to);
   if (clear) {
      VkClearColorValue c = { .float32 = { 0.25f, 0.5f, 0.75f, 1.0f } };
      VkImageSubresourceRange r = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 };
      vkCmdClearColorImage(cmd, im->img, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                           &c, 1, &r);
      image_barrier(cmd, im->img, VK_PIPELINE_STAGE_2_ALL_TRANSFER_BIT,
                    VK_ACCESS_2_TRANSFER_WRITE_BIT,
                    VK_PIPELINE_STAGE_2_ALL_COMMANDS_BIT,
                    VK_ACCESS_2_MEMORY_READ_BIT | VK_ACCESS_2_MEMORY_WRITE_BIT,
                    VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, to);
   }
   CHECK(vkEndCommandBuffer(cmd));
   submit_wait(cmd);
   vkFreeCommandBuffers(dev, cpool, 1, &cmd);
}

static void
begin_render(VkCommandBuffer cmd, const struct img *rt, bool clear)
{
   VkRenderingAttachmentInfo ca = {
      .sType = VK_STRUCTURE_TYPE_RENDERING_ATTACHMENT_INFO,
      .imageView = rt->view,
      .imageLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
      .loadOp = clear ? VK_ATTACHMENT_LOAD_OP_CLEAR : VK_ATTACHMENT_LOAD_OP_LOAD,
      .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
   };
   VkRenderingInfo ri = {
      .sType = VK_STRUCTURE_TYPE_RENDERING_INFO,
      .renderArea = { { 0, 0 }, { rt->w, rt->h } },
      .layerCount = 1,
      .colorAttachmentCount = 1,
      .pColorAttachments = &ca,
   };
   vkCmdBeginRendering(cmd, &ri);
   VkViewport vp = { 0, 0, (float)rt->w, (float)rt->h, 0, 1 };
   VkRect2D sc = { { 0, 0 }, { rt->w, rt->h } };
   vkCmdSetViewport(cmd, 0, 1, &vp);
   vkCmdSetScissor(cmd, 0, 1, &sc);
}

static void
push(VkCommandBuffer cmd, const struct pc *c)
{
   vkCmdPushConstants(cmd, pl, VK_SHADER_STAGE_ALL, 0, sizeof(*c), c);
}

/* ------------------------------------------------------------------ */
/* fullscreen tests */

struct fs_ctx {
   VkPipeline p;
   VkDescriptorSet set;
   const struct img *rt;
   int passes;
   struct pc pc;
   bool vary_pass; /* pc.u.y = pass index */
};

static void
fs_body(VkCommandBuffer cmd, void *v)
{
   struct fs_ctx *c = v;
   begin_render(cmd, c->rt, true);
   vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, c->p);
   if (c->set)
      vkCmdBindDescriptorSets(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, pl, 0, 1,
                              &c->set, 0, NULL);
   struct pc pc = c->pc;
   for (int i = 0; i < c->passes; i++) {
      if (i == 0 || c->vary_pass) {
         pc.u[1] = i;
         push(cmd, &pc);
      }
      vkCmdDraw(cmd, 3, 1, 0, 0);
   }
   vkCmdEndRendering(cmd);
}

static double
run_fs(VkPipeline p, VkDescriptorSet set, const struct img *rt, int passes,
       struct pc pc, bool vary)
{
   struct fs_ctx c = { p, set, rt, passes, pc, vary };
   return time_body(fs_body, &c);
}

static void
test_alu(bool alu, bool fma)
{
   const VkImageUsageFlags u = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT;
   struct img rt;
   make_image(&rt, VK_FORMAT_R8G8B8A8_UNORM, W, H, u, true);
   init_layout(&rt, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL, false);
   const double px = (double)W * H;

   if (alu) {
      VkPipeline pa = make_gfx(SHADER(spv_alu_frag), rt.fmt, false);
      struct pc pc = { { 0.5f, 0, 0, 0 }, { 64, 0, 0, 0 } };
      double t = run_fs(pa, VK_NULL_HANDLE, &rt, 4, pc, false);
      report("alu 64 iter x4", t, px * 4 / 1e9, "Gpix/s");
      vkDestroyPipeline(dev, pa, NULL);
   }
   if (fma) {
      VkPipeline pf = make_gfx(SHADER(spv_fma_frag), rt.fmt, false);
      struct pc pc = { { 0.5f, 0.25f, 0.75f, 0.125f }, { 256, 0, 0, 0 } };
      double t = run_fs(pf, VK_NULL_HANDLE, &rt, 4, pc, false);
      report("fma 256 iter x4", t, px * 4 * 256 * 32 / 1e12, "TFLOP/s");
      vkDestroyPipeline(dev, pf, NULL);
   }
   free_image(&rt);
}

static void
test_fill(void)
{
   const VkImageUsageFlags u = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT |
                               VK_IMAGE_USAGE_SAMPLED_BIT |
                               VK_IMAGE_USAGE_TRANSFER_SRC_BIT;
   const double px = (double)W * H;
   VkShaderModule m_color = SHADER(spv_color_frag);
   VkShaderModule m_noise = SHADER(spv_noise_frag);
   VkPipeline p8c = make_gfx(m_color, VK_FORMAT_R8G8B8A8_UNORM, false);
   VkPipeline p8n = make_gfx(m_noise, VK_FORMAT_R8G8B8A8_UNORM, false);
   VkPipeline p16b = make_gfx(m_noise, VK_FORMAT_R16G16B16A16_SFLOAT, true);

   for (int ded = 1; ded >= 0; ded--) {
      const char *where = ded ? "dedicated" : "placed";
      struct img rt8, rt16;
      make_image(&rt8, VK_FORMAT_R8G8B8A8_UNORM, W, H, u, ded);
      make_image(&rt16, VK_FORMAT_R16G16B16A16_SFLOAT, W, H, u, ded);
      init_layout(&rt8, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL, false);
      init_layout(&rt16, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL, false);
      char name[64];
      struct pc pc = { { 0.2f, 0.4f, 0.6f, 0.5f }, { 0 } };
      snprintf(name, sizeof(name), "fill rgba8 const %s", where);
      report(name, run_fs(p8c, VK_NULL_HANDLE, &rt8, 16, pc, false), px * 16 / 1e9, "Gpix/s");
      snprintf(name, sizeof(name), "fill rgba8 noise %s", where);
      report(name, run_fs(p8n, VK_NULL_HANDLE, &rt8, 16, pc, true), px * 16 / 1e9, "Gpix/s");
      snprintf(name, sizeof(name), "blend rgba16f noise %s", where);
      report(name, run_fs(p16b, VK_NULL_HANDLE, &rt16, 16, pc, true), px * 16 / 1e9, "Gpix/s");
      free_image(&rt8);
      free_image(&rt16);
      if (!ded)
         reset_heap();
   }
   vkDestroyPipeline(dev, p8c, NULL);
   vkDestroyPipeline(dev, p8n, NULL);
   vkDestroyPipeline(dev, p16b, NULL);
}

static void
test_tex(void)
{
   const double px = (double)W * H;
   struct img rt, t4k, t8k;
   make_image(&rt, VK_FORMAT_R8G8B8A8_UNORM, W, H,
              VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT, true);
   init_layout(&rt, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL, false);
   const VkImageUsageFlags tu = VK_IMAGE_USAGE_SAMPLED_BIT |
                                VK_IMAGE_USAGE_TRANSFER_DST_BIT;
   make_image(&t4k, VK_FORMAT_R8G8B8A8_UNORM, 4096, 4096, tu, true);
   make_image(&t8k, VK_FORMAT_R8G8B8A8_UNORM, 8192, 8192, tu, true);
   init_layout(&t4k, VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL, true);
   init_layout(&t8k, VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL, true);
   VkDescriptorSet s4k = make_set(&t4k, NULL, NULL);
   VkDescriptorSet s8k = make_set(&t8k, NULL, NULL);

   VkPipeline pt = make_gfx(SHADER(spv_tex_frag), rt.fmt, false);
   VkPipeline pr = make_gfx(SHADER(spv_texrand_frag), rt.fmt, false);
   /* cached: the 8 taps of neighbouring pixels overlap within a few texels */
   struct pc pc = { { 1.0f / 4096 / 4, 1.0f / 4096 / 4, 0.5f / 4096, 0.5f / 4096 }, { 0 } };
   report("tex 8 taps 4k rgba8 cached x4", run_fs(pt, s4k, &rt, 4, pc, false),
          px * 4 * 8 / 1e9, "Gtap/s");
   /* streaming: ~1.6 texels per pixel, the 8 taps 64 texels apart */
   pc = (struct pc){ { 1.6f / 4096, 1.6f / 4096, 64.0f / 4096, 0 }, { 0 } };
   report("tex 8 taps 4k rgba8 stream x4", run_fs(pt, s4k, &rt, 4, pc, false),
          px * 4 * 8 / 1e9, "Gtap/s");
   pc = (struct pc){ { 1.0f / 8192, 1.0f / 8192, 0, 0 }, { 0 } };
   report("tex random 8x8 blocks 256MiB x4", run_fs(pr, s8k, &rt, 4, pc, true),
          px * 4 * 4 / 1e9, "Gtap/s");

   vkDestroyPipeline(dev, pt, NULL);
   vkDestroyPipeline(dev, pr, NULL);
   free_image(&rt);
   free_image(&t4k);
   free_image(&t8k);
   CHECK(vkResetDescriptorPool(dev, dpool, 0));
}

/* ------------------------------------------------------------------ */
/* compute */

struct cs_ctx {
   VkPipeline p;
   VkDescriptorSet sets[2];
   int dispatches;
   uint32_t groups;
   struct pc pc;
   int barrier; /* 0 none, 1 compute->compute, 2 ALL_COMMANDS */
};

static void
cs_body(VkCommandBuffer cmd, void *v)
{
   struct cs_ctx *c = v;
   vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_COMPUTE, c->p);
   push(cmd, &c->pc);
   for (int i = 0; i < c->dispatches; i++) {
      vkCmdBindDescriptorSets(cmd, VK_PIPELINE_BIND_POINT_COMPUTE, pl, 0, 1,
                              &c->sets[i & 1], 0, NULL);
      vkCmdDispatch(cmd, c->groups, 1, 1);
      if (c->barrier == 1)
         memory_barrier(cmd, VK_PIPELINE_STAGE_2_COMPUTE_SHADER_BIT,
                        VK_ACCESS_2_SHADER_STORAGE_WRITE_BIT,
                        VK_PIPELINE_STAGE_2_COMPUTE_SHADER_BIT,
                        VK_ACCESS_2_SHADER_STORAGE_READ_BIT |
                        VK_ACCESS_2_SHADER_STORAGE_WRITE_BIT);
      else if (c->barrier == 2)
         memory_barrier(cmd, VK_PIPELINE_STAGE_2_ALL_COMMANDS_BIT,
                        VK_ACCESS_2_MEMORY_WRITE_BIT,
                        VK_PIPELINE_STAGE_2_ALL_COMMANDS_BIT,
                        VK_ACCESS_2_MEMORY_READ_BIT | VK_ACCESS_2_MEMORY_WRITE_BIT);
   }
}

static VkPipeline p_copy, p_spin;

static void
test_bw(void)
{
   const VkDeviceSize size = 256ull << 20;
   struct buf a, b;
   make_buffer(&a, size);
   make_buffer(&b, size);
   VkDescriptorSet s = make_set(NULL, &a, &b);
   const uint32_t n = (uint32_t)(size / 16);
   struct cs_ctx c = { p_copy, { s, s }, 4, (n + 255) / 256,
                       { { 0 }, { n, 0, 0, 0 } }, 0 };
   double t = time_body(cs_body, &c);
   report("copy 256 MiB compute x4", t, 2.0 * size * 4 / 1e9, "GB/s");
   vkDestroyBuffer(dev, a.buf, NULL);
   vkFreeMemory(dev, a.mem, NULL);
   vkDestroyBuffer(dev, b.buf, NULL);
   vkFreeMemory(dev, b.mem, NULL);
   CHECK(vkResetDescriptorPool(dev, dpool, 0));
}

static void
test_barrier(void)
{
   const VkDeviceSize size = 4ull << 20;
   struct buf a, b;
   make_buffer(&a, size);
   make_buffer(&b, size);
   VkDescriptorSet ab = make_set(NULL, &a, &b), ba = make_set(NULL, &b, &a);
   const uint32_t n = (uint32_t)(size / 16);
   const int k = 256;
   struct cs_ctx c = { p_copy, { ab, ba }, k, (n + 255) / 256,
                       { { 0 }, { n, 0, 0, 0 } }, 0 };
   double t0 = time_body(cs_body, &c);
   c.barrier = 1;
   double t1 = time_body(cs_body, &c);
   c.barrier = 2;
   double t2 = time_body(cs_body, &c);
   report("barrier: 256 x 4 MiB copy, none", t0, 0, "");
   printf("%-34s gpu %9.3f ms  %10.2f us/barrier\n",
          "barrier: compute->compute", t1, (t1 - t0) * 1e3 / k);
   printf("%-34s gpu %9.3f ms  %10.2f us/barrier\n",
          "barrier: ALL_COMMANDS", t2, (t2 - t0) * 1e3 / k);
   fflush(stdout);
   vkDestroyBuffer(dev, a.buf, NULL);
   vkFreeMemory(dev, a.mem, NULL);
   vkDestroyBuffer(dev, b.buf, NULL);
   vkFreeMemory(dev, b.mem, NULL);
   CHECK(vkResetDescriptorPool(dev, dpool, 0));
}

/* ------------------------------------------------------------------ */
/* render pass splits */

struct rp_ctx {
   VkPipeline p;
   struct img *rt[2];
   VkDescriptorSet read[2]; /* read[i]: samples rt[i] */
   VkDescriptorSet fixed;   /* samples a texture nobody writes */
   int passes;
   bool split;
};

static void
rp_body(VkCommandBuffer cmd, void *v)
{
   struct rp_ctx *c = v;
   struct pc pc = { { 1.0f / c->rt[0]->w, 1.0f / c->rt[0]->h, 0, 0 }, { 0 } };
   if (!c->split) {
      begin_render(cmd, c->rt[0], false);
      vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, c->p);
      push(cmd, &pc);
      vkCmdBindDescriptorSets(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, pl, 0, 1,
                              &c->fixed, 0, NULL);
      for (int i = 0; i < c->passes; i++)
         vkCmdDraw(cmd, 3, 1, 0, 0);
      vkCmdEndRendering(cmd);
      return;
   }
   /* rt[0] starts as a colour attachment, rt[1] as a shader-read image */
   for (int i = 0; i < c->passes; i++) {
      struct img *dst = c->rt[i & 1];
      begin_render(cmd, dst, false);
      vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, c->p);
      push(cmd, &pc);
      vkCmdBindDescriptorSets(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, pl, 0, 1,
                              &c->read[(i + 1) & 1], 0, NULL);
      vkCmdDraw(cmd, 3, 1, 0, 0);
      vkCmdEndRendering(cmd);
      /* dst becomes the next pass's source, the other its target */
      image_barrier(cmd, dst->img,
                    VK_PIPELINE_STAGE_2_COLOR_ATTACHMENT_OUTPUT_BIT,
                    VK_ACCESS_2_COLOR_ATTACHMENT_WRITE_BIT,
                    VK_PIPELINE_STAGE_2_FRAGMENT_SHADER_BIT,
                    VK_ACCESS_2_SHADER_SAMPLED_READ_BIT,
                    VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
                    VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL);
      image_barrier(cmd, c->rt[(i + 1) & 1]->img,
                    VK_PIPELINE_STAGE_2_FRAGMENT_SHADER_BIT, 0,
                    VK_PIPELINE_STAGE_2_COLOR_ATTACHMENT_OUTPUT_BIT,
                    VK_ACCESS_2_COLOR_ATTACHMENT_WRITE_BIT,
                    VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL,
                    VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL);
   }
   /* an even pass count leaves both images as they started */
}

static void
test_rtpass(void)
{
   const VkImageUsageFlags u = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT |
                               VK_IMAGE_USAGE_SAMPLED_BIT |
                               VK_IMAGE_USAGE_TRANSFER_DST_BIT;
   struct img a, b, t;
   make_image(&a, VK_FORMAT_R8G8B8A8_UNORM, 960, 540, u, false);
   make_image(&b, VK_FORMAT_R8G8B8A8_UNORM, 960, 540, u, false);
   make_image(&t, VK_FORMAT_R8G8B8A8_UNORM, 960, 540, u, false);
   init_layout(&a, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL, true);
   init_layout(&b, VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL, true);
   init_layout(&t, VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL, true);
   VkPipeline p = make_gfx(SHADER(spv_blit_frag), a.fmt, false);
   struct rp_ctx c = { p, { &a, &b }, { make_set(&a, NULL, NULL), make_set(&b, NULL, NULL) },
                       make_set(&t, NULL, NULL), 256, false };
   double t1 = time_body(rp_body, &c);
   c.split = true;
   double t2 = time_body(rp_body, &c);
   report("rtpass: 256 draws 960x540, 1 pass", t1, 0, "");
   printf("%-34s gpu %9.3f ms  %10.2f us/pass\n",
          "rtpass: 256 passes + transitions", t2, (t2 - t1) * 1e3 / 256);
   fflush(stdout);
   vkDestroyPipeline(dev, p, NULL);
   free_image(&a);
   free_image(&b);
   free_image(&t);
   reset_heap();
   CHECK(vkResetDescriptorPool(dev, dpool, 0));
}

/* ------------------------------------------------------------------ */
/* submission */

static VkSemaphore timeline;
static uint64_t timeline_value;

static void
timeline_wait(uint64_t v)
{
   VkSemaphoreWaitInfo wi = {
      .sType = VK_STRUCTURE_TYPE_SEMAPHORE_WAIT_INFO,
      .semaphoreCount = 1, .pSemaphores = &timeline, .pValues = &v,
   };
   CHECK(vkWaitSemaphores(dev, &wi, 10ull * 1000 * 1000 * 1000));
}

static void
submit_signal(VkCommandBuffer cmd)
{
   VkCommandBufferSubmitInfo ci = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_SUBMIT_INFO,
      .commandBuffer = cmd,
   };
   VkSemaphoreSubmitInfo si = {
      .sType = VK_STRUCTURE_TYPE_SEMAPHORE_SUBMIT_INFO,
      .semaphore = timeline, .value = ++timeline_value,
      .stageMask = VK_PIPELINE_STAGE_2_ALL_COMMANDS_BIT,
   };
   VkSubmitInfo2 s = {
      .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO_2,
      .commandBufferInfoCount = 1, .pCommandBufferInfos = &ci,
      .signalSemaphoreInfoCount = 1, .pSignalSemaphoreInfos = &si,
   };
   CHECK(vkQueueSubmit2(queue, 1, &s, VK_NULL_HANDLE));
}

/* spin.comp iterations for one dispatch of `groups` groups to take ~us */
static uint32_t
calibrate_spin(VkDescriptorSet set, uint32_t groups, double us)
{
   uint32_t iters = 256;
   for (int round = 0; round < 4; round++) {
      struct cs_ctx c = { p_spin, { set, set }, 1, groups,
                          { { 0 }, { iters, 0, 0, 0 } }, 0 };
      double t = time_body(cs_body, &c) * 1e3;
      if (t <= 0)
         t = 1;
      double scale = us / t;
      uint64_t next = (uint64_t)(iters * scale);
      iters = (uint32_t)MIN2(MAX2(next, 16), 1u << 24);
   }
   return iters;
}

static void
test_submit(void)
{
   struct buf out;
   make_buffer(&out, 1 << 20);
   VkDescriptorSet set = make_set(NULL, NULL, &out);
   const uint32_t groups = 2048; /* 128k threads: fills the GPU */
   const double unit_us = getenv("BENCH_UNIT_US") ? atof(getenv("BENCH_UNIT_US")) : 50.0;
   const uint32_t iters = calibrate_spin(set, groups, unit_us);
   const int units = 64, inflight = 2;
   static const int splits[] = { 1, 4, 16, 64 };

   printf("# submit: %d units/frame, unit ~%.0f us (%u iterations), %d frames in flight\n",
          units, unit_us, iters, inflight);
   for (unsigned si = 0; si < sizeof(splits) / sizeof(splits[0]); si++) {
      const int nsub = splits[si];
      const int per = units / nsub;
      /* command buffers [slot][submit], queries 2 per submit per slot */
      VkCommandBuffer cmds[2][64];
      for (int slot = 0; slot < inflight; slot++) {
         for (int s = 0; s < nsub; s++) {
            VkCommandBufferAllocateInfo ai = {
               .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
               .commandPool = cpool,
               .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
               .commandBufferCount = 1,
            };
            CHECK(vkAllocateCommandBuffers(dev, &ai, &cmds[slot][s]));
            VkCommandBuffer cmd = cmds[slot][s];
            VkCommandBufferBeginInfo bi = {
               .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
            };
            CHECK(vkBeginCommandBuffer(cmd, &bi));
            const uint32_t q = (slot * 64 + s) * 2;
            vkCmdResetQueryPool(cmd, qpool, q, 2);
            ts_write(cmd, q, false);
            vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_COMPUTE, p_spin);
            vkCmdBindDescriptorSets(cmd, VK_PIPELINE_BIND_POINT_COMPUTE, pl, 0,
                                    1, &set, 0, NULL);
            struct pc pc = { { 0 }, { iters, 0, 0, 0 } };
            push(cmd, &pc);
            for (int k = 0; k < per; k++)
               vkCmdDispatch(cmd, groups, 1, 1);
            ts_write(cmd, q + 1, true);
            CHECK(vkEndCommandBuffer(cmd));
         }
      }

      uint64_t frame_end[2] = { 0, 0 };
      double busy_sum = 0, gap_sum = 0;
      int gap_n = 0, frames = 0, measured = 0;
      uint64_t prev_end = 0;
      const double t_start = now_ms();
      double t_measure = 0;
      for (;;) {
         const int slot = frames % inflight;
         if (frame_end[slot]) {
            timeline_wait(frame_end[slot]);
            uint64_t ts[128];
            CHECK(vkGetQueryPoolResults(dev, qpool, slot * 128, nsub * 2,
                                        sizeof(ts), ts, 8,
                                        VK_QUERY_RESULT_64_BIT));
            if (frames >= 2 * inflight + 4) { /* past the warm-up */
               if (measured == 0)
                  t_measure = now_ms();
               for (int s = 0; s < nsub; s++) {
                  busy_sum += ts_ms(ts[2 * s], ts[2 * s + 1]);
                  uint64_t begin = ts[2 * s];
                  if (prev_end && begin > prev_end) {
                     gap_sum += ts_ms(prev_end, begin);
                     gap_n++;
                  } else if (prev_end) {
                     gap_n++;
                  }
                  prev_end = ts[2 * s + 1];
               }
               measured++;
            } else {
               prev_end = ts[2 * nsub - 1];
            }
         }
         if (now_ms() - t_start > 2000 && measured > 8)
            break;
         for (int s = 0; s < nsub; s++)
            submit_signal(cmds[slot][s]);
         frame_end[slot] = timeline_value;
         frames++;
      }
      const double wall = now_ms() - t_measure;
      CHECK(vkQueueWaitIdle(queue));
      /* the first measured frame's wait started the clock */
      const double wall_per = wall / MAX2(measured - 1, 1);
      const double busy_per = busy_sum / measured;
      char name[64];
      snprintf(name, sizeof(name), "submit %2d/frame", nsub);
      printf("%-34s wall %8.3f ms/frame  busy %8.3f ms  idle %5.1f%%  gap %7.1f us/submit\n",
             name, wall_per, busy_per, 100.0 * (1.0 - busy_per / wall_per),
             gap_n ? gap_sum * 1e3 / gap_n : 0.0);
      fflush(stdout);
      for (int slot = 0; slot < inflight; slot++)
         vkFreeCommandBuffers(dev, cpool, nsub, cmds[slot]);
   }
   vkDestroyBuffer(dev, out.buf, NULL);
   vkFreeMemory(dev, out.mem, NULL);
   CHECK(vkResetDescriptorPool(dev, dpool, 0));
}


/* ------------------------------------------------------------------ */
/* CPU side of recording: d3d12_gpu_bound's record test in Vulkan.  Per
 * frame 8 command buffers x 64 tiny dispatches, 2 frames in flight; once
 * with the pools reset and reused, once with a new pool per command buffer
 * every frame (what vkd3d-proton does for an app that makes new command
 * allocators per frame).
 */
static void
test_record(void)
{
   struct buf out;
   make_buffer(&out, 1 << 20);
   VkDescriptorSet set = make_set(NULL, &out, &out);
   enum { LISTS = 8, DISPATCHES = 64, INFLIGHT = 2 };
   for (int fresh = 0; fresh <= 1; fresh++) {
      VkCommandPool pools[INFLIGHT][LISTS];
      VkCommandBuffer cmds[INFLIGHT][LISTS];
      memset(pools, 0, sizeof(pools));
      uint64_t slot_value[INFLIGHT] = { 0 };
      double rec_ms = 0, t_measure = 0, pool_ms = 0;
      int frames = 0, measured = 0;
      const double t0 = now_ms();
      for (;;) {
         const int slot = frames % INFLIGHT;
         if (slot_value[slot])
            timeline_wait(slot_value[slot]);
         if (frames == 8)
            t_measure = now_ms();
         if (now_ms() - t0 > 2500 && measured > 8)
            break;
         const double r0 = now_ms();
         double p_ms = 0;
         for (int k = 0; k < LISTS; k++) {
            const double p0 = now_ms();
            if (pools[slot][k] && fresh) {
               vkDestroyCommandPool(dev, pools[slot][k], NULL);
               pools[slot][k] = VK_NULL_HANDLE;
            }
            if (!pools[slot][k]) {
               VkCommandPoolCreateInfo pci = {
                  .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
                  .queueFamilyIndex = qfam,
               };
               CHECK(vkCreateCommandPool(dev, &pci, NULL, &pools[slot][k]));
               VkCommandBufferAllocateInfo ai = {
                  .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
                  .commandPool = pools[slot][k],
                  .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
                  .commandBufferCount = 1,
               };
               CHECK(vkAllocateCommandBuffers(dev, &ai, &cmds[slot][k]));
            } else {
               CHECK(vkResetCommandPool(dev, pools[slot][k], 0));
            }
            p_ms += now_ms() - p0;
            VkCommandBuffer cmd = cmds[slot][k];
            VkCommandBufferBeginInfo bi = {
               .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
               .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
            };
            CHECK(vkBeginCommandBuffer(cmd, &bi));
            vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_COMPUTE, p_copy);
            vkCmdBindDescriptorSets(cmd, VK_PIPELINE_BIND_POINT_COMPUTE, pl, 0,
                                    1, &set, 0, NULL);
            for (int d = 0; d < DISPATCHES; d++) {
               struct pc pc = { { (float)d, 0, 0, 0 }, { 256, d, 0, 0 } };
               push(cmd, &pc);
               vkCmdDispatch(cmd, 1, 1, 1);
            }
            CHECK(vkEndCommandBuffer(cmd));
            submit_signal(cmd);
         }
         slot_value[slot] = timeline_value;
         if (frames >= 8) {
            rec_ms += now_ms() - r0;
            pool_ms += p_ms;
            measured++;
         }
         frames++;
      }
      const double wall = now_ms() - t_measure;
      CHECK(vkQueueWaitIdle(queue));
      for (int s = 0; s < INFLIGHT; s++)
         for (int k = 0; k < LISTS; k++)
            if (pools[s][k])
               vkDestroyCommandPool(dev, pools[s][k], NULL);
      printf("%-34s wall %8.3f ms/frame  record+submit %8.3f ms/frame  "
             "pools %7.3f ms  %6.2f us/dispatch\n",
             fresh ? "record new pools" : "record reused pools",
             wall / measured, rec_ms / measured, pool_ms / measured,
             rec_ms * 1e3 / measured / (LISTS * DISPATCHES));
      fflush(stdout);
   }
   vkDestroyBuffer(dev, out.buf, NULL);
   vkFreeMemory(dev, out.mem, NULL);
   CHECK(vkResetDescriptorPool(dev, dpool, 0));
}

/* ------------------------------------------------------------------ */
/* sustained load */

static void
test_sustain(void)
{
   struct img rt;
   make_image(&rt, VK_FORMAT_R8G8B8A8_UNORM, W, H,
              VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT, true);
   init_layout(&rt, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL, false);
   VkPipeline pa = make_gfx(SHADER(spv_alu_frag), rt.fmt, false);

   /* ~4 ms per submit */
   uint32_t iters = 16;
   for (int round = 0; round < 3; round++) {
      struct pc pc = { { 0.5f, 0, 0, 0 }, { iters, 0, 0, 0 } };
      double t = run_fs(pa, VK_NULL_HANDLE, &rt, 1, pc, false);
      iters = (uint32_t)MIN2(MAX2(iters * 4.0 / MAX2(t, 0.01), 1), 1 << 20);
   }

   enum { INFLIGHT = 3, MAXS = 20000 };
   VkCommandBuffer cmds[INFLIGHT];
   for (int slot = 0; slot < INFLIGHT; slot++) {
      cmds[slot] = begin_cmd();
      VkCommandBuffer cmd = cmds[slot];
      vkCmdResetQueryPool(cmd, qpool, slot * 2, 2);
      ts_write(cmd, slot * 2, false);
      struct fs_ctx c = { pa, VK_NULL_HANDLE, &rt, 1,
                          { { 0.5f, 0, 0, 0 }, { iters, 0, 0, 0 } }, false };
      fs_body(cmd, &c);
      ts_write(cmd, slot * 2 + 1, true);
      CHECK(vkEndCommandBuffer(cmd));
   }

   double *gpu = malloc(sizeof(double) * MAXS);
   uint64_t first_begin = 0, last_end = 0;
   uint64_t slot_value[INFLIGHT] = { 0 };
   int n = 0, submitted = 0, sec_start_n = 0;
   const double t0 = now_ms();
   double sec_t = t0;
   printf("# sustain: %u iterations per submit, %d in flight, %.0f s\n", iters,
          INFLIGHT, sustain_s);
   for (;;) {
      const int slot = submitted % INFLIGHT;
      if (slot_value[slot]) {
         timeline_wait(slot_value[slot]);
         uint64_t ts[2];
         CHECK(vkGetQueryPoolResults(dev, qpool, slot * 2, 2, sizeof(ts), ts, 8,
                                     VK_QUERY_RESULT_64_BIT));
         if (n < MAXS) {
            if (!first_begin)
               first_begin = ts[0];
            last_end = ts[1];
            gpu[n++] = ts_ms(ts[0], ts[1]);
         }
      }
      const double t = now_ms();
      if (t - sec_t >= 1000.0 && n > sec_start_n) {
         double *w = malloc(sizeof(double) * (n - sec_start_n));
         memcpy(w, gpu + sec_start_n, sizeof(double) * (n - sec_start_n));
         int m = n - sec_start_n;
         double mn = pct(w, m, 0.0), med = pct(w, m, 0.5), mx = pct(w, m, 1.0);
         printf("  t=%5.1fs  %4d submits  gpu min %.3f  p50 %.3f  max %.3f ms\n",
                (t - t0) / 1e3, m, mn, med, mx);
         fflush(stdout);
         free(w);
         sec_start_n = n;
         sec_t = t;
      }
      if (t - t0 >= sustain_s * 1000.0 || n >= MAXS)
         break;
      submit_signal(cmds[slot]);
      slot_value[slot] = timeline_value;
      submitted++;
   }
   CHECK(vkQueueWaitIdle(queue));
   double sum = 0;
   for (int i = 0; i < n; i++)
      sum += gpu[i];
   const double span = ts_ms(first_begin, last_end);
   double mn = pct(gpu, n, 0.0), p50 = pct(gpu, n, 0.5),
          p99 = pct(gpu, n, 0.99), mx = pct(gpu, n, 1.0);
   printf("%-34s gpu min %.3f  p50 %.3f  p99 %.3f  max %.3f ms; "
          "p50/min %.3f; busy %.1f%% of the GPU span\n",
          "sustain alu 4k", mn, p50, p99, mx, p50 / mn,
          span > 0 ? 100.0 * sum / span : 0.0);
   fflush(stdout);
   free(gpu);
   vkFreeCommandBuffers(dev, cpool, INFLIGHT, cmds);
   vkDestroyPipeline(dev, pa, NULL);
   free_image(&rt);
}

/* ------------------------------------------------------------------ */

static bool
want(const char *tests, const char *t)
{
   if (tests == NULL)
      return true;
   size_t n = strlen(t);
   for (const char *p = tests; (p = strstr(p, t)) != NULL; p += n) {
      if ((p == tests || p[-1] == ',') && (p[n] == ',' || p[n] == '\0'))
         return true;
   }
   return false;
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
      else if (!strcmp(argv[i], "-w") && i + 1 < argc)
         W = atoi(argv[++i]);
      else if (!strcmp(argv[i], "-h") && i + 1 < argc)
         H = atoi(argv[++i]);
      else if (!strcmp(argv[i], "-s") && i + 1 < argc)
         sustain_s = atof(argv[++i]);
      else {
         fprintf(stderr, "usage: %s [-r reps] [-t test,...] [-w W -h H] [-s seconds]\n",
                 argv[0]);
         return 2;
      }
   }
   reps = MIN2(MAX2(reps, 1), 64);

   VkApplicationInfo app = {
      .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .pApplicationName = "vk_gpu_bound",
      .apiVersion = VK_API_VERSION_1_3,
   };
   VkInstanceCreateInfo ici = {
      .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
      .pApplicationInfo = &app,
   };
   direct_driver_chain(&ici);
   CHECK(vkCreateInstance(&ici, NULL, &inst));
   uint32_t n = 8;
   VkPhysicalDevice pdevs[8];
   CHECK(vkEnumeratePhysicalDevices(inst, &n, pdevs));
   const char *want_name = getenv("BENCH_DEVICE");
   pdev = VK_NULL_HANDLE;
   for (uint32_t i = 0; i < n && !pdev; i++) {
      vkGetPhysicalDeviceProperties(pdevs[i], &props);
      if (want_name ? strstr(props.deviceName, want_name) != NULL
                    : props.deviceType == VK_PHYSICAL_DEVICE_TYPE_DISCRETE_GPU)
         pdev = pdevs[i];
   }
   if (pdev == VK_NULL_HANDLE) {
      fprintf(stderr, "no matching GPU\n");
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
   printf("# vk_gpu_bound: %s / %s %s, %ux%u, median of %d\n", props.deviceName,
          drv.driverName, drv.driverInfo, W, H, reps);
   const char *knobs[] = { "NVK_RM_TIMESLICE_US", "NVK_RM_COMPRESSION",
                           "NVK_RM_COMPRESS_ALL", "NVK_RM_COMPRESS_TYPE",
                           "NVK_RM_HUGE_PAGES", "NVK_DEBUG", NULL };
   for (int i = 0; knobs[i]; i++)
      if (getenv(knobs[i]))
         printf("# %s=%s\n", knobs[i], getenv(knobs[i]));

   uint32_t nq = 8;
   VkQueueFamilyProperties qf[8];
   vkGetPhysicalDeviceQueueFamilyProperties(pdev, &nq, qf);
   for (qfam = 0; qfam < nq; qfam++)
      if (qf[qfam].queueFlags & VK_QUEUE_GRAPHICS_BIT)
         break;
   if (qfam == nq || qf[qfam].timestampValidBits == 0) {
      fprintf(stderr, "no graphics queue with timestamps\n");
      return 1;
   }
   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = qfam, .queueCount = 1, .pQueuePriorities = &prio,
   };
   VkPhysicalDeviceVulkan13Features f13 = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES,
      .synchronization2 = VK_TRUE,
      .dynamicRendering = VK_TRUE,
   };
   VkPhysicalDeviceVulkan12Features f12 = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES,
      .pNext = &f13,
      .timelineSemaphore = VK_TRUE,
      .hostQueryReset = VK_TRUE,
   };
   VkDeviceCreateInfo dci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
      .pNext = &f12,
      .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci,
   };
   CHECK(vkCreateDevice(pdev, &dci, NULL, &dev));
   vkGetDeviceQueue(dev, qfam, 0, &queue);

   VkCommandPoolCreateInfo cpci = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
      .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
      .queueFamilyIndex = qfam,
   };
   CHECK(vkCreateCommandPool(dev, &cpci, NULL, &cpool));
   VkQueryPoolCreateInfo qpci = {
      .sType = VK_STRUCTURE_TYPE_QUERY_POOL_CREATE_INFO,
      .queryType = VK_QUERY_TYPE_TIMESTAMP, .queryCount = NQUERIES,
   };
   CHECK(vkCreateQueryPool(dev, &qpci, NULL, &qpool));
   vkResetQueryPool(dev, qpool, 0, NQUERIES);
   VkSemaphoreTypeCreateInfo stci = {
      .sType = VK_STRUCTURE_TYPE_SEMAPHORE_TYPE_CREATE_INFO,
      .semaphoreType = VK_SEMAPHORE_TYPE_TIMELINE,
   };
   VkSemaphoreCreateInfo sci = {
      .sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO, .pNext = &stci,
   };
   CHECK(vkCreateSemaphore(dev, &sci, NULL, &timeline));

   VkDescriptorSetLayoutBinding b[3] = {
      { 0, VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, 1, VK_SHADER_STAGE_ALL, NULL },
      { 1, VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 1, VK_SHADER_STAGE_ALL, NULL },
      { 2, VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 1, VK_SHADER_STAGE_ALL, NULL },
   };
   VkDescriptorSetLayoutCreateInfo dslci = {
      .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
      .bindingCount = 3, .pBindings = b,
   };
   CHECK(vkCreateDescriptorSetLayout(dev, &dslci, NULL, &dsl));
   VkPushConstantRange pcr = { VK_SHADER_STAGE_ALL, 0, sizeof(struct pc) };
   VkPipelineLayoutCreateInfo plci = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
      .setLayoutCount = 1, .pSetLayouts = &dsl,
      .pushConstantRangeCount = 1, .pPushConstantRanges = &pcr,
   };
   CHECK(vkCreatePipelineLayout(dev, &plci, NULL, &pl));
   VkDescriptorPoolSize ps[2] = {
      { VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, 64 },
      { VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 128 },
   };
   VkDescriptorPoolCreateInfo dpci = {
      .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
      .maxSets = 64, .poolSizeCount = 2, .pPoolSizes = ps,
   };
   CHECK(vkCreateDescriptorPool(dev, &dpci, NULL, &dpool));
   VkSamplerCreateInfo smci = {
      .sType = VK_STRUCTURE_TYPE_SAMPLER_CREATE_INFO,
      .magFilter = VK_FILTER_LINEAR, .minFilter = VK_FILTER_LINEAR,
      .addressModeU = VK_SAMPLER_ADDRESS_MODE_REPEAT,
      .addressModeV = VK_SAMPLER_ADDRESS_MODE_REPEAT,
      .addressModeW = VK_SAMPLER_ADDRESS_MODE_REPEAT,
   };
   CHECK(vkCreateSampler(dev, &smci, NULL, &sampler));
   p_copy = make_comp(SHADER(spv_copy_comp));
   p_spin = make_comp(SHADER(spv_spin_comp));

   if (want(tests, "alu") || want(tests, "fma"))
      test_alu(want(tests, "alu"), want(tests, "fma"));
   if (want(tests, "fill"))
      test_fill();
   if (want(tests, "tex"))
      test_tex();
   if (want(tests, "bw"))
      test_bw();
   if (want(tests, "barrier"))
      test_barrier();
   if (want(tests, "rtpass"))
      test_rtpass();
   if (want(tests, "record"))
      test_record();
   if (want(tests, "submit"))
      test_submit();
   if (want(tests, "sustain"))
      test_sustain();

   CHECK(vkDeviceWaitIdle(dev));
   printf("# done\n");
   return 0;
}
