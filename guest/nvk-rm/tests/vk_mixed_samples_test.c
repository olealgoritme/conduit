/* vk_mixed_samples_test: VK_NV_framebuffer_mixed_samples, rasterization at
 * more samples than the color attachment has (what DXVK uses for D3D11.1
 * target-independent rasterization), checked pixel by pixel.
 *
 * A single-sample RGBA8 target, one triangle whose slanted edge cuts pixels
 * at every coverage fraction, a fragment shader that writes white:
 *
 *   - raster 2, 4, 8 and 16 samples, coverage modulation off: every covered
 *     pixel is white (a color sample is covered if any of its raster samples
 *     is);
 *   - the same with linear RGBA modulation: interior pixels white, edge
 *     pixels at every level k/N, k = 1..N-1;
 *   - the same with a modulation table requested: NVK does not use the
 *     hardware's coefficient table (a class error on GB202 with the tables
 *     tried) and modulates linearly, so the linear levels again;
 *   - raster 16 recorded in a secondary command buffer whose inheritance
 *     names the color sample count with VkAttachmentSampleCountInfoAMD;
 *   - a single-sample depth attachment with depth test off and raster 16
 *     (no target-independent rasterization with a depth attachment: the
 *     draw rasterizes at one sample, at the one-sample position, so the
 *     result must equal a plain single-sample draw pixel for pixel);
 *   - color 1 sample, depth 4 samples, raster 4, and color 4 samples,
 *     depth 1 sample, raster 4, with depth clears (not supported by the
 *     driver: the device must just survive them, every access staying
 *     inside both images).
 *
 * NVK exposes VK_NV_framebuffer_mixed_samples only where driconf
 * nvk_mixed_samples is on (DXVK); run this test with nvk_mixed_samples=true
 * in the environment.
 *
 * Prints PASS/FAIL per case and a RESULT line.  Picks the device by name
 * (default "NVK").  A few small draws.
 */
#ifdef _WIN32
#include <windows.h>
#endif
#include <vulkan/vulkan.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>

#include "vk_direct_driver.h"
#include "mixed_samples_vert.h"
#include "mixed_samples_frag.h"

#define CHECK(x) do { VkResult r_ = (x); if (r_ != VK_SUCCESS) { \
   fprintf(stderr, "FAIL %s:%d: %s -> %d\n", __FILE__, __LINE__, #x, r_); exit(1); } } while (0)

#define W 128
#define H 128

static VkPhysicalDeviceMemoryProperties memprops;
static VkDevice dev;
static VkQueue queue;
static VkCommandPool pool;
static VkPipelineLayout layout;
static VkShaderModule vs_mod, fs_mod;
static VkImage color_img, color4_img;
static VkImageView color_view, color4_view;
static VkBuffer readback;
static uint8_t *readback_map;

static uint32_t find_mem(uint32_t bits, VkMemoryPropertyFlags want)
{
   for (uint32_t i = 0; i < memprops.memoryTypeCount; i++)
      if ((bits & (1u << i)) && (memprops.memoryTypes[i].propertyFlags & want) == want)
         return i;
   fprintf(stderr, "FAIL no memory type\n");
   exit(1);
}

static VkImage make_image(VkFormat format, VkSampleCountFlagBits samples, VkImageUsageFlags usage,
                          VkImageView *view, VkImageAspectFlags aspect)
{
   VkImageCreateInfo ii = { VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO, .imageType = VK_IMAGE_TYPE_2D,
      .format = format, .extent = { W, H, 1 }, .mipLevels = 1, .arrayLayers = 1,
      .samples = samples, .tiling = VK_IMAGE_TILING_OPTIMAL, .usage = usage };
   VkImage img;
   CHECK(vkCreateImage(dev, &ii, NULL, &img));
   VkMemoryRequirements mr;
   vkGetImageMemoryRequirements(dev, img, &mr);
   VkMemoryAllocateInfo mai = { VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = mr.size,
      .memoryTypeIndex = find_mem(mr.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT) };
   VkDeviceMemory m;
   CHECK(vkAllocateMemory(dev, &mai, NULL, &m));
   CHECK(vkBindImageMemory(dev, img, m, 0));
   VkImageViewCreateInfo vci = { VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO, .image = img,
      .viewType = VK_IMAGE_VIEW_TYPE_2D, .format = format,
      .subresourceRange = { aspect, 0, 1, 0, 1 } };
   CHECK(vkCreateImageView(dev, &vci, NULL, view));
   return img;
}

struct pipe_desc {
   VkSampleCountFlagBits raster;
   VkSampleCountFlagBits color_samples;  /* 0: 1 sample */
   int clear_depth_in_pass;        /* vkCmdClearAttachments on the depth */
   VkFormat depth_format;          /* VK_FORMAT_UNDEFINED: no depth attachment */
   VkSampleCountFlagBits depth_samples;
   VkBool32 depth_test;
   VkCoverageModulationModeNV mode;
   uint32_t table_count;           /* 0: linear */
   const float *table;
};

static VkPipeline make_pipeline(const struct pipe_desc *d)
{
   VkPipelineShaderStageCreateInfo st[2] = {
      { VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, .stage = VK_SHADER_STAGE_VERTEX_BIT,
        .module = vs_mod, .pName = "main" },
      { VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, .stage = VK_SHADER_STAGE_FRAGMENT_BIT,
        .module = fs_mod, .pName = "main" },
   };
   VkPipelineVertexInputStateCreateInfo vis = { VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO };
   VkPipelineInputAssemblyStateCreateInfo ias = { VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
      .topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST };
   VkViewport vp = { 0, 0, W, H, 0, 1 };
   VkRect2D sc = { { 0, 0 }, { W, H } };
   VkPipelineViewportStateCreateInfo vps = { VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO,
      .viewportCount = 1, .pViewports = &vp, .scissorCount = 1, .pScissors = &sc };
   VkPipelineRasterizationStateCreateInfo rs = { VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO,
      .polygonMode = VK_POLYGON_MODE_FILL, .cullMode = VK_CULL_MODE_NONE, .lineWidth = 1.0f };
   VkPipelineCoverageModulationStateCreateInfoNV cm = {
      VK_STRUCTURE_TYPE_PIPELINE_COVERAGE_MODULATION_STATE_CREATE_INFO_NV,
      .coverageModulationMode = d->mode,
      .coverageModulationTableEnable = d->table_count != 0,
      .coverageModulationTableCount = d->table_count,
      .pCoverageModulationTable = d->table };
   VkSampleMask mask = ~0u;
   VkPipelineMultisampleStateCreateInfo ms = { VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO,
      &cm, .rasterizationSamples = d->raster, .pSampleMask = &mask };
   VkPipelineDepthStencilStateCreateInfo ds = { VK_STRUCTURE_TYPE_PIPELINE_DEPTH_STENCIL_STATE_CREATE_INFO,
      .depthTestEnable = d->depth_test, .depthWriteEnable = d->depth_test,
      .depthCompareOp = VK_COMPARE_OP_LESS_OR_EQUAL };
   VkPipelineColorBlendAttachmentState cba = { .colorWriteMask = 0xf };
   VkPipelineColorBlendStateCreateInfo cbs = { VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO,
      .attachmentCount = 1, .pAttachments = &cba };
   VkSampleCountFlagBits color_samples = d->color_samples ? d->color_samples : VK_SAMPLE_COUNT_1_BIT;
   VkAttachmentSampleCountInfoAMD asc = { VK_STRUCTURE_TYPE_ATTACHMENT_SAMPLE_COUNT_INFO_AMD,
      .colorAttachmentCount = 1, .pColorAttachmentSamples = &color_samples,
      .depthStencilAttachmentSamples = d->depth_format ? d->depth_samples : 0 };
   VkFormat cf = VK_FORMAT_R8G8B8A8_UNORM;
   VkPipelineRenderingCreateInfo prci = { VK_STRUCTURE_TYPE_PIPELINE_RENDERING_CREATE_INFO, &asc,
      .colorAttachmentCount = 1, .pColorAttachmentFormats = &cf, .depthAttachmentFormat = d->depth_format };
   VkGraphicsPipelineCreateInfo gpci = { VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO, &prci,
      .stageCount = 2, .pStages = st, .pVertexInputState = &vis, .pInputAssemblyState = &ias,
      .pViewportState = &vps, .pRasterizationState = &rs, .pMultisampleState = &ms,
      .pDepthStencilState = &ds, .pColorBlendState = &cbs, .layout = layout };
   VkPipeline p;
   CHECK(vkCreateGraphicsPipelines(dev, VK_NULL_HANDLE, 1, &gpci, NULL, &p));
   return p;
}

static void barrier(VkCommandBuffer cb, VkImage img, VkImageAspectFlags aspect, VkImageLayout from,
                    VkImageLayout to, VkAccessFlags src, VkAccessFlags dst)
{
   VkImageMemoryBarrier b = { VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER, .srcAccessMask = src,
      .dstAccessMask = dst, .oldLayout = from, .newLayout = to,
      .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
      .image = img, .subresourceRange = { aspect, 0, 1, 0, 1 } };
   vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT,
                        0, 0, NULL, 0, NULL, 1, &b);
}

/* Draw with `pipe` (in a secondary if `secondary`), read the alpha channel
 * back into `alpha` (single-sample color only).  Returns false if the
 * device was lost.
 */
static int run(VkPipeline pipe, const struct pipe_desc *d, int secondary, uint8_t *alpha)
{
   const int color4 = d->color_samples == VK_SAMPLE_COUNT_4_BIT;
   VkImage cimg = color4 ? color4_img : color_img;
   VkImageView cview = color4 ? color4_view : color_view;
   VkImage depth_img = VK_NULL_HANDLE;
   VkImageView depth_view = VK_NULL_HANDLE;
   if (d->depth_format)
      depth_img = make_image(d->depth_format, d->depth_samples,
                             VK_IMAGE_USAGE_DEPTH_STENCIL_ATTACHMENT_BIT, &depth_view,
                             VK_IMAGE_ASPECT_DEPTH_BIT);

   VkCommandBufferAllocateInfo cbai = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
      .commandPool = pool, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1 };
   VkCommandBuffer cb, sec = VK_NULL_HANDLE;
   CHECK(vkAllocateCommandBuffers(dev, &cbai, &cb));
   VkCommandBufferBeginInfo cbbi = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
      .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT };
   CHECK(vkBeginCommandBuffer(cb, &cbbi));

   barrier(cb, cimg, VK_IMAGE_ASPECT_COLOR_BIT, VK_IMAGE_LAYOUT_UNDEFINED,
           VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL, 0, VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT);
   if (depth_img)
      barrier(cb, depth_img, VK_IMAGE_ASPECT_DEPTH_BIT, VK_IMAGE_LAYOUT_UNDEFINED,
              VK_IMAGE_LAYOUT_DEPTH_ATTACHMENT_OPTIMAL, 0, VK_ACCESS_DEPTH_STENCIL_ATTACHMENT_WRITE_BIT);

   VkRenderingAttachmentInfo ca = { VK_STRUCTURE_TYPE_RENDERING_ATTACHMENT_INFO,
      .imageView = cview, .imageLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
      .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR, .storeOp = VK_ATTACHMENT_STORE_OP_STORE };
   VkRenderingAttachmentInfo da = { VK_STRUCTURE_TYPE_RENDERING_ATTACHMENT_INFO,
      .imageView = depth_view, .imageLayout = VK_IMAGE_LAYOUT_DEPTH_ATTACHMENT_OPTIMAL,
      .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR, .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
      .clearValue.depthStencil.depth = 1.0f };
   VkRenderingInfo ri = { VK_STRUCTURE_TYPE_RENDERING_INFO,
      .flags = secondary ? VK_RENDERING_CONTENTS_SECONDARY_COMMAND_BUFFERS_BIT : 0,
      .renderArea = { { 0, 0 }, { W, H } }, .layerCount = 1,
      .colorAttachmentCount = 1, .pColorAttachments = &ca,
      .pDepthAttachment = depth_img ? &da : NULL };
   vkCmdBeginRendering(cb, &ri);

   if (secondary) {
      VkSampleCountFlagBits color_samples = VK_SAMPLE_COUNT_1_BIT;
      VkFormat cf = VK_FORMAT_R8G8B8A8_UNORM;
      VkCommandBufferInheritanceRenderingInfo inr = {
         VK_STRUCTURE_TYPE_COMMAND_BUFFER_INHERITANCE_RENDERING_INFO,
         .colorAttachmentCount = 1, .pColorAttachmentFormats = &cf,
         .depthAttachmentFormat = d->depth_format, .rasterizationSamples = d->raster };
      VkAttachmentSampleCountInfoAMD asc = { VK_STRUCTURE_TYPE_ATTACHMENT_SAMPLE_COUNT_INFO_AMD, &inr,
         .colorAttachmentCount = 1, .pColorAttachmentSamples = &color_samples,
         .depthStencilAttachmentSamples = d->depth_format ? d->depth_samples : 0 };
      VkCommandBufferInheritanceInfo inh = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_INHERITANCE_INFO, &asc };
      VkCommandBufferAllocateInfo sai = cbai;
      sai.level = VK_COMMAND_BUFFER_LEVEL_SECONDARY;
      CHECK(vkAllocateCommandBuffers(dev, &sai, &sec));
      VkCommandBufferBeginInfo sbi = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
         .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT |
                  VK_COMMAND_BUFFER_USAGE_RENDER_PASS_CONTINUE_BIT,
         .pInheritanceInfo = &inh };
      CHECK(vkBeginCommandBuffer(sec, &sbi));
      vkCmdBindPipeline(sec, VK_PIPELINE_BIND_POINT_GRAPHICS, pipe);
      vkCmdDraw(sec, 3, 1, 0, 0);
      CHECK(vkEndCommandBuffer(sec));
      vkCmdExecuteCommands(cb, 1, &sec);
   } else {
      vkCmdBindPipeline(cb, VK_PIPELINE_BIND_POINT_GRAPHICS, pipe);
      vkCmdDraw(cb, 3, 1, 0, 0);
      if (d->clear_depth_in_pass) {
         VkClearAttachment ca_d = { VK_IMAGE_ASPECT_DEPTH_BIT, .clearValue.depthStencil.depth = 0.25f };
         VkClearRect cr = { { { 0, 0 }, { W, H } }, 0, 1 };
         vkCmdClearAttachments(cb, 1, &ca_d, 1, &cr);
         vkCmdDraw(cb, 3, 1, 0, 0);
      }
   }
   vkCmdEndRendering(cb);

   if (!color4) {
   barrier(cb, color_img, VK_IMAGE_ASPECT_COLOR_BIT, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
           VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
           VK_ACCESS_TRANSFER_READ_BIT);
   VkBufferImageCopy copy = { .imageSubresource = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1 },
      .imageExtent = { W, H, 1 } };
   vkCmdCopyImageToBuffer(cb, color_img, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, readback, 1, &copy);
   VkMemoryBarrier host = { VK_STRUCTURE_TYPE_MEMORY_BARRIER, .srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
      .dstAccessMask = VK_ACCESS_HOST_READ_BIT };
   vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_HOST_BIT,
                        0, 1, &host, 0, NULL, 0, NULL);
   }
   CHECK(vkEndCommandBuffer(cb));

   VkFenceCreateInfo fci = { VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
   VkFence fence;
   CHECK(vkCreateFence(dev, &fci, NULL, &fence));
   VkSubmitInfo si = { VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cb };
   VkResult r = vkQueueSubmit(queue, 1, &si, fence);
   if (r == VK_SUCCESS)
      r = vkWaitForFences(dev, 1, &fence, VK_TRUE, 5ull * 1000 * 1000 * 1000);
   if (r != VK_SUCCESS) {
      printf("submit/wait -> %d\n", r);
      return 0;
   }
   for (uint32_t i = 0; i < W * H; i++)
      alpha[i] = color4 ? 0 : readback_map[i * 4 + 3];
   vkDestroyFence(dev, fence, NULL);
   vkFreeCommandBuffers(dev, pool, 1, &cb);
   if (sec)
      vkFreeCommandBuffers(dev, pool, 1, &sec);
   if (depth_img) {
      vkDestroyImageView(dev, depth_view, NULL);
      vkDestroyImage(dev, depth_img, NULL);
   }
   return 1;
}

static uint8_t unorm8(float f)
{
   return (uint8_t)lrintf(f * 255.0f);
}

/* Interior pixel: deep inside the triangle, fully covered. */
#define INTERIOR (alpha[(H - 8) * W + W / 2])

static int failures;

static void report(const char *name, int ok, const char *detail)
{
   printf("%-34s %s  %s\n", name, detail, ok ? "PASS" : "FAIL");
   failures += !ok;
}

/* Levels present among the covered pixels, as a 256-bit set. */
static void levels(const uint8_t *alpha, int set[256], uint32_t *covered)
{
   memset(set, 0, 256 * sizeof(int));
   *covered = 0;
   for (uint32_t i = 0; i < W * H; i++) {
      set[alpha[i]] = 1;
      *covered += alpha[i] != 0;
   }
}

int main(int argc, char **argv)
{
   const char *want = argc > 1 ? argv[1] : "NVK";

   VkApplicationInfo app = { VK_STRUCTURE_TYPE_APPLICATION_INFO, .apiVersion = VK_API_VERSION_1_3 };
   VkInstanceCreateInfo ici = { VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app };
   direct_driver_chain(&ici);
   VkInstance inst;
   CHECK(vkCreateInstance(&ici, NULL, &inst));
   uint32_t npd = 8;
   VkPhysicalDevice pds[8], pd = VK_NULL_HANDLE;
   CHECK(vkEnumeratePhysicalDevices(inst, &npd, pds));
   VkPhysicalDeviceProperties props;
   for (uint32_t i = 0; i < npd; i++) {
      vkGetPhysicalDeviceProperties(pds[i], &props);
      if (!pd && strstr(props.deviceName, want))
         pd = pds[i];
   }
   if (!pd) {
      fprintf(stderr, "FAIL no device matching '%s'\n", want);
      return 1;
   }
   vkGetPhysicalDeviceProperties(pd, &props);
   vkGetPhysicalDeviceMemoryProperties(pd, &memprops);
   printf("using: %s\n", props.deviceName);

   uint32_t next = 0;
   vkEnumerateDeviceExtensionProperties(pd, NULL, &next, NULL);
   VkExtensionProperties *exts = calloc(next, sizeof(*exts));
   vkEnumerateDeviceExtensionProperties(pd, NULL, &next, exts);
   int have = 0;
   for (uint32_t i = 0; i < next; i++)
      have |= !strcmp(exts[i].extensionName, VK_NV_FRAMEBUFFER_MIXED_SAMPLES_EXTENSION_NAME);
   if (!have) {
      printf("VK_NV_framebuffer_mixed_samples not supported (NVK: set nvk_mixed_samples=true)\n"
             "RESULT: SKIP\n");
      return 0;
   }
   const char *dev_exts[] = { VK_NV_FRAMEBUFFER_MIXED_SAMPLES_EXTENSION_NAME };

   uint32_t qf = 0, nqf = 16;
   VkQueueFamilyProperties qfp[16];
   vkGetPhysicalDeviceQueueFamilyProperties(pd, &nqf, qfp);
   for (qf = 0; qf < nqf; qf++)
      if (qfp[qf].queueFlags & VK_QUEUE_GRAPHICS_BIT)
         break;
   VkPhysicalDeviceVulkan13Features f13 = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES,
      .dynamicRendering = VK_TRUE };
   VkPhysicalDeviceFeatures2 f2 = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2, &f13 };
   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = { VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = qf, .queueCount = 1, .pQueuePriorities = &prio };
   VkDeviceCreateInfo dci = { VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, &f2,
      .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci,
      .enabledExtensionCount = 1, .ppEnabledExtensionNames = dev_exts };
   CHECK(vkCreateDevice(pd, &dci, NULL, &dev));
   vkGetDeviceQueue(dev, qf, 0, &queue);

   VkCommandPoolCreateInfo cpi = { VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .queueFamilyIndex = qf };
   CHECK(vkCreateCommandPool(dev, &cpi, NULL, &pool));
   VkPipelineLayoutCreateInfo plci = { VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO };
   CHECK(vkCreatePipelineLayout(dev, &plci, NULL, &layout));
   VkShaderModuleCreateInfo smci = { VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
      .codeSize = sizeof(mixed_samples_vert), .pCode = mixed_samples_vert };
   CHECK(vkCreateShaderModule(dev, &smci, NULL, &vs_mod));
   smci.codeSize = sizeof(mixed_samples_frag);
   smci.pCode = mixed_samples_frag;
   CHECK(vkCreateShaderModule(dev, &smci, NULL, &fs_mod));

   color_img = make_image(VK_FORMAT_R8G8B8A8_UNORM, VK_SAMPLE_COUNT_1_BIT,
                          VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT,
                          &color_view, VK_IMAGE_ASPECT_COLOR_BIT);
   color4_img = make_image(VK_FORMAT_R8G8B8A8_UNORM, VK_SAMPLE_COUNT_4_BIT,
                           VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT, &color4_view, VK_IMAGE_ASPECT_COLOR_BIT);
   VkBufferCreateInfo bci = { VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size = W * H * 4,
      .usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT };
   CHECK(vkCreateBuffer(dev, &bci, NULL, &readback));
   VkMemoryRequirements mr;
   vkGetBufferMemoryRequirements(dev, readback, &mr);
   VkMemoryAllocateInfo mai = { VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = mr.size,
      .memoryTypeIndex = find_mem(mr.memoryTypeBits, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                                                     VK_MEMORY_PROPERTY_HOST_COHERENT_BIT) };
   VkDeviceMemory bm;
   CHECK(vkAllocateMemory(dev, &mai, NULL, &bm));
   CHECK(vkBindBufferMemory(dev, readback, bm, 0));
   CHECK(vkMapMemory(dev, bm, 0, W * H * 4, 0, (void **)&readback_map));

   static uint8_t alpha[W * H];
   int set[256];
   uint32_t covered;
   char detail[256], name[64];

   const VkSampleCountFlagBits rasters[] = {
      VK_SAMPLE_COUNT_2_BIT, VK_SAMPLE_COUNT_4_BIT, VK_SAMPLE_COUNT_8_BIT, VK_SAMPLE_COUNT_16_BIT };
   for (uint32_t r = 0; r < 4; r++) {
      const uint32_t n = rasters[r];

      /* No modulation: all covered pixels white */
      struct pipe_desc d = { .raster = n, .mode = VK_COVERAGE_MODULATION_MODE_NONE_NV };
      VkPipeline p = make_pipeline(&d);
      if (!run(p, &d, 0, alpha))
         return 2;
      levels(alpha, set, &covered);
      int partial = 0;
      for (int a = 1; a < 255; a++)
         partial += set[a];
      snprintf(name, sizeof(name), "raster %2u, no modulation", n);
      snprintf(detail, sizeof(detail), "%5u covered, %d partial levels", covered, partial);
      report(name, covered > 8000 && partial == 0, detail);
      vkDestroyPipeline(dev, p, NULL);

      /* Linear modulation: levels k/n */
      d.mode = VK_COVERAGE_MODULATION_MODE_RGBA_NV;
      p = make_pipeline(&d);
      if (!run(p, &d, 0, alpha))
         return 2;
      levels(alpha, set, &covered);
      int expected = 0, unexpected = 0;
      for (int a = 1; a < 256; a++) {
         if (!set[a])
            continue;
         int match = 0;
         for (uint32_t k = 1; k <= n; k++)
            match |= abs(a - (int)unorm8((float)k / n)) <= 1;
         if (match)
            expected++;
         else
            unexpected++;
      }
      snprintf(name, sizeof(name), "raster %2u, linear modulation", n);
      snprintf(detail, sizeof(detail), "interior %3u, %5u covered, %d levels k/%u, %d others",
               INTERIOR, covered, expected, n, unexpected);
      report(name, INTERIOR == 255 && expected == (int)n && unexpected == 0, detail);
      vkDestroyPipeline(dev, p, NULL);

      /* A modulation table: NVK does not program the hardware's
       * coefficient table (SET_TIR_MODULATION_COEFFICIENT_TABLE with a
       * non-monotonic table was a class error on GB202, Xid 69 error code 4)
       * and modulates linearly instead.  The device must survive it and
       * show the linear levels.
       */
      float table[16];
      for (uint32_t i = 0; i < n; i++)
         table[i] = (float)(((i * 5 + 3) % n) + 1) / (float)(n + 1);
      d.table_count = n;
      d.table = table;
      p = make_pipeline(&d);
      if (!run(p, &d, 0, alpha))
         return 2;
      levels(alpha, set, &covered);
      expected = 0;
      unexpected = 0;
      for (int a = 1; a < 256; a++) {
         if (!set[a])
            continue;
         int match = 0;
         for (uint32_t k = 1; k <= n; k++)
            match |= abs(a - (int)unorm8((float)k / n)) <= 1;
         if (match)
            expected++;
         else
            unexpected++;
      }
      snprintf(name, sizeof(name), "raster %2u, table (linear in NVK)", n);
      snprintf(detail, sizeof(detail), "interior %3u, %d levels k/%u, %d others",
               INTERIOR, expected, n, unexpected);
      report(name, INTERIOR == 255 && expected == (int)n && unexpected == 0, detail);
      vkDestroyPipeline(dev, p, NULL);
   }

   /* Secondary command buffer, inheritance with the attachment sample counts */
   {
      struct pipe_desc d = { .raster = VK_SAMPLE_COUNT_16_BIT, .mode = VK_COVERAGE_MODULATION_MODE_RGBA_NV };
      VkPipeline p = make_pipeline(&d);
      if (!run(p, &d, 1, alpha))
         return 2;
      levels(alpha, set, &covered);
      int partial = 0;
      for (int a = 1; a < 255; a++)
         partial += set[a];
      snprintf(detail, sizeof(detail), "interior %3u, %5u covered, %d partial levels", INTERIOR, covered,
               partial);
      report("raster 16 in a secondary", INTERIOR == 255 && partial == 15, detail);
      vkDestroyPipeline(dev, p, NULL);
   }

   /* Single-sample depth attachment, tests off, raster 16: no
    * target-independent rasterization, so exactly a single-sample draw
    */
   {
      static uint8_t ref[W * H];
      struct pipe_desc r1 = { .raster = VK_SAMPLE_COUNT_1_BIT, .mode = VK_COVERAGE_MODULATION_MODE_NONE_NV };
      VkPipeline p = make_pipeline(&r1);
      if (!run(p, &r1, 0, ref))
         return 2;
      vkDestroyPipeline(dev, p, NULL);

      struct pipe_desc d = { .raster = VK_SAMPLE_COUNT_16_BIT, .depth_format = VK_FORMAT_D32_SFLOAT,
                             .depth_samples = VK_SAMPLE_COUNT_1_BIT,
                             .mode = VK_COVERAGE_MODULATION_MODE_RGBA_NV };
      p = make_pipeline(&d);
      if (!run(p, &d, 0, alpha))
         return 2;
      uint32_t diff = 0, ref_covered = 0;
      for (uint32_t i = 0; i < W * H; i++) {
         diff += alpha[i] != ref[i];
         ref_covered += ref[i] != 0;
      }
      levels(alpha, set, &covered);
      snprintf(detail, sizeof(detail), "%5u covered (1x draw %5u), %u pixels differ", covered,
               ref_covered, diff);
      report("raster 16, 1x depth attached", diff == 0 && ref_covered > 8000, detail);
      vkDestroyPipeline(dev, p, NULL);
   }

   /* Color 1 sample, depth 4 samples, raster 4: unsupported, must survive */
   {
      struct pipe_desc d = { .raster = VK_SAMPLE_COUNT_4_BIT, .depth_format = VK_FORMAT_D32_SFLOAT,
                             .depth_samples = VK_SAMPLE_COUNT_4_BIT, .depth_test = VK_TRUE,
                             .clear_depth_in_pass = 1,
                             .mode = VK_COVERAGE_MODULATION_MODE_NONE_NV };
      VkPipeline p = make_pipeline(&d);
      int alive = run(p, &d, 0, alpha);
      levels(alpha, set, &covered);
      snprintf(detail, sizeof(detail), "device %s, %5u covered", alive ? "alive" : "LOST", covered);
      report("color 1x, depth 4x, raster 4", alive, detail);
      if (!alive)
         return 2;
      vkDestroyPipeline(dev, p, NULL);
   }

   /* Color 4 samples, depth 1 sample, raster 4, depth cleared by the
    * load op and by vkCmdClearAttachments: unsupported, must survive
    */
   {
      struct pipe_desc d = { .raster = VK_SAMPLE_COUNT_4_BIT, .color_samples = VK_SAMPLE_COUNT_4_BIT,
                             .depth_format = VK_FORMAT_D32_SFLOAT,
                             .depth_samples = VK_SAMPLE_COUNT_1_BIT, .clear_depth_in_pass = 1,
                             .mode = VK_COVERAGE_MODULATION_MODE_NONE_NV };
      VkPipeline p = make_pipeline(&d);
      int alive = run(p, &d, 0, alpha);
      snprintf(detail, sizeof(detail), "device %s", alive ? "alive" : "LOST");
      report("color 4x, depth 1x, raster 4", alive, detail);
      if (!alive)
         return 2;
      vkDestroyPipeline(dev, p, NULL);
   }

   printf("RESULT: %s\n", failures ? "FAIL" : "PASS");
   return failures ? 2 : 0;
}
