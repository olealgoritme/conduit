/* vk_draw_fetch_test: vertex fetch and draw parameters the way DXVK draws CS2.
 *
 * Bindings as in CS2's depth prepass: b0 per-vertex stride 12, b1 per-instance
 * R32_UINT stride 4 (divisor 1), b2 null (stride 0, per instance, divisor 0,
 * RGBA32UI), strides dynamic (vkCmdBindVertexBuffers2).  Points with
 * rasterizer discard; the vertex shader appends one record per invocation
 * (tag, vertex/instance index, base instance/vertex, fetched attributes) and
 * the CPU checks every record against the draw that produced it.  Direct,
 * indexed, indirect, indirect-count draws, pipelines that do and do not read
 * gl_BaseInstance/gl_BaseVertex, rebinding at offsets, a blit (meta) and a
 * render pass break in between.  A few hundred vertices: microseconds.
 */
#ifdef _WIN32
#include <windows.h>
#endif
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>

#include "draw_fetch_vert.h"

#define CHECK(x) do { VkResult r_ = (x); if (r_ != VK_SUCCESS) { \
   fprintf(stderr, "FAIL %s:%d: %s -> %d\n", __FILE__, __LINE__, #x, r_); exit(1); } } while (0)

#define MAXREC 16384
#define NTAG 16

static VkPhysicalDeviceMemoryProperties memprops;
static VkDevice dev;

static uint32_t find_mem(uint32_t bits, VkMemoryPropertyFlags want)
{
   for (uint32_t i = 0; i < memprops.memoryTypeCount; i++)
      if ((bits & (1u << i)) && (memprops.memoryTypes[i].propertyFlags & want) == want)
         return i;
   fprintf(stderr, "FAIL no memory type\n");
   exit(1);
}

static VkBuffer make_buf(VkDeviceSize size, VkBufferUsageFlags usage, void **map)
{
   VkBufferCreateInfo bci = { VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size = size, .usage = usage };
   VkBuffer b;
   CHECK(vkCreateBuffer(dev, &bci, NULL, &b));
   VkMemoryRequirements mr;
   vkGetBufferMemoryRequirements(dev, b, &mr);
   VkMemoryAllocateInfo mai = { VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = mr.size,
      .memoryTypeIndex = find_mem(mr.memoryTypeBits, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                                                     VK_MEMORY_PROPERTY_HOST_COHERENT_BIT) };
   VkDeviceMemory m;
   CHECK(vkAllocateMemory(dev, &mai, NULL, &m));
   CHECK(vkBindBufferMemory(dev, b, m, 0));
   CHECK(vkMapMemory(dev, m, 0, size, 0, map));
   return b;
}

static VkImage make_img(void)
{
   VkImageCreateInfo ii = { VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO, .imageType = VK_IMAGE_TYPE_2D,
      .format = VK_FORMAT_R8G8B8A8_UNORM, .extent = { 16, 16, 1 }, .mipLevels = 1, .arrayLayers = 1,
      .samples = VK_SAMPLE_COUNT_1_BIT, .tiling = VK_IMAGE_TILING_OPTIMAL,
      .usage = VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT |
               VK_IMAGE_USAGE_SAMPLED_BIT | VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT };
   VkImage img;
   CHECK(vkCreateImage(dev, &ii, NULL, &img));
   VkMemoryRequirements mr;
   vkGetImageMemoryRequirements(dev, img, &mr);
   VkMemoryAllocateInfo mai = { VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = mr.size,
      .memoryTypeIndex = find_mem(mr.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT) };
   VkDeviceMemory m;
   CHECK(vkAllocateMemory(dev, &mai, NULL, &m));
   CHECK(vkBindImageMemory(dev, img, m, 0));
   return img;
}

struct tag_info {
   const char *what;
   uint32_t voff, ioff;   /* element offsets b0 and b1 are bound at */
   int reads_bases;
   uint32_t expected;     /* invocations */
   /* allowed (base instance, base vertex) pairs, and instance count */
   uint32_t npairs;
   uint32_t bi[4], bv[4];
   uint32_t inst_count;
   uint32_t seen, bad;
};

static struct tag_info tags[NTAG];
static uint32_t ntags;

int main(int argc, char **argv)
{
   const char *want = argc > 1 ? argv[1] : "NVK";

   VkApplicationInfo app = { VK_STRUCTURE_TYPE_APPLICATION_INFO, .apiVersion = VK_API_VERSION_1_3 };
   VkInstanceCreateInfo ici = { VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app };
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
   int khr_r2 = 0, khr_div = 0;
   for (uint32_t i = 0; i < next; i++) {
      khr_r2 |= !strcmp(exts[i].extensionName, "VK_KHR_robustness2");
      khr_div |= !strcmp(exts[i].extensionName, "VK_KHR_vertex_attribute_divisor");
   }
   const char *dev_exts[] = {
      khr_r2 ? "VK_KHR_robustness2" : "VK_EXT_robustness2",
      khr_div ? "VK_KHR_vertex_attribute_divisor" : "VK_EXT_vertex_attribute_divisor",
      "VK_EXT_extended_dynamic_state",
   };

   uint32_t qf = 0, nqf = 16;
   VkQueueFamilyProperties qfp[16];
   vkGetPhysicalDeviceQueueFamilyProperties(pd, &nqf, qfp);
   for (qf = 0; qf < nqf; qf++)
      if (qfp[qf].queueFlags & VK_QUEUE_GRAPHICS_BIT)
         break;

   VkPhysicalDeviceVertexAttributeDivisorFeaturesKHR divf = {
      VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VERTEX_ATTRIBUTE_DIVISOR_FEATURES_KHR,
      .vertexAttributeInstanceRateDivisor = VK_TRUE, .vertexAttributeInstanceRateZeroDivisor = VK_TRUE };
   VkPhysicalDeviceRobustness2FeaturesEXT r2 = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ROBUSTNESS_2_FEATURES_EXT,
      &divf, .robustBufferAccess2 = VK_TRUE, .robustImageAccess2 = VK_TRUE, .nullDescriptor = VK_TRUE };
   VkPhysicalDeviceExtendedDynamicStateFeaturesEXT eds = {
      VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTENDED_DYNAMIC_STATE_FEATURES_EXT, &r2, .extendedDynamicState = VK_TRUE };
   VkPhysicalDeviceVulkan13Features f13 = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES, &eds,
      .dynamicRendering = VK_TRUE, .robustImageAccess = VK_TRUE };
   VkPhysicalDeviceVulkan12Features f12 = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES, &f13,
      .drawIndirectCount = VK_TRUE };
   VkPhysicalDeviceVulkan11Features f11 = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_1_FEATURES, &f12,
      .shaderDrawParameters = VK_TRUE };
   VkPhysicalDeviceFeatures2 f2 = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2, &f11 };
   f2.features.robustBufferAccess = VK_TRUE;
   f2.features.vertexPipelineStoresAndAtomics = VK_TRUE;
   f2.features.multiDrawIndirect = VK_TRUE;
   f2.features.drawIndirectFirstInstance = VK_TRUE;

   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = { VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = qf, .queueCount = 1, .pQueuePriorities = &prio };
   VkDeviceCreateInfo dci = { VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, &f2,
      .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci,
      .enabledExtensionCount = 3, .ppEnabledExtensionNames = dev_exts };
   CHECK(vkCreateDevice(pd, &dci, NULL, &dev));
   VkQueue q;
   vkGetDeviceQueue(dev, qf, 0, &q);

   void *m_out, *m_pos, *m_inst, *m_idx, *m_ind;
   VkBuffer out_buf = make_buf(16 + (VkDeviceSize)MAXREC * 48, VK_BUFFER_USAGE_STORAGE_BUFFER_BIT |
                               VK_BUFFER_USAGE_TRANSFER_DST_BIT, &m_out);
   VkBuffer pos_buf = make_buf(4096 * 12, VK_BUFFER_USAGE_VERTEX_BUFFER_BIT, &m_pos);
   VkBuffer inst_buf = make_buf(4096 * 4, VK_BUFFER_USAGE_VERTEX_BUFFER_BIT, &m_inst);
   VkBuffer idx_buf = make_buf(256 * 4, VK_BUFFER_USAGE_INDEX_BUFFER_BIT, &m_idx);
   VkBuffer ind_buf = make_buf(4096, VK_BUFFER_USAGE_INDIRECT_BUFFER_BIT, &m_ind);
   memset(m_out, 0, 16 + (size_t)MAXREC * 48);
   uint32_t *pos = m_pos, *ib = m_inst, *idx = m_idx;
   for (uint32_t i = 0; i < 4096; i++) {
      pos[i * 3 + 0] = i;
      pos[i * 3 + 1] = i ^ 0x5a5a5a5au;
      pos[i * 3 + 2] = 7;
      ib[i] = i + 0x10000u;
   }
   for (uint32_t i = 0; i < 256; i++)
      idx[i] = (i * 7) % 64;

   VkDescriptorSetLayoutBinding b0 = { 0, VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 1, VK_SHADER_STAGE_VERTEX_BIT };
   VkDescriptorSetLayoutCreateInfo dslci = { VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
      .bindingCount = 1, .pBindings = &b0 };
   VkDescriptorSetLayout dsl;
   CHECK(vkCreateDescriptorSetLayout(dev, &dslci, NULL, &dsl));
   VkDescriptorPoolSize ps = { VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 1 };
   VkDescriptorPoolCreateInfo dpci = { VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
      .maxSets = 1, .poolSizeCount = 1, .pPoolSizes = &ps };
   VkDescriptorPool dp;
   CHECK(vkCreateDescriptorPool(dev, &dpci, NULL, &dp));
   VkDescriptorSetAllocateInfo dsai = { VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
      .descriptorPool = dp, .descriptorSetCount = 1, .pSetLayouts = &dsl };
   VkDescriptorSet ds;
   CHECK(vkAllocateDescriptorSets(dev, &dsai, &ds));
   VkDescriptorBufferInfo obi = { out_buf, 0, VK_WHOLE_SIZE };
   VkWriteDescriptorSet w = { VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = ds, .dstBinding = 0,
      .descriptorCount = 1, .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, .pBufferInfo = &obi };
   vkUpdateDescriptorSets(dev, 1, &w, 0, NULL);

   VkPushConstantRange pcr = { VK_SHADER_STAGE_VERTEX_BIT, 0, 16 };
   VkPipelineLayoutCreateInfo plci = { VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
      .setLayoutCount = 1, .pSetLayouts = &dsl, .pushConstantRangeCount = 1, .pPushConstantRanges = &pcr };
   VkPipelineLayout pl;
   CHECK(vkCreatePipelineLayout(dev, &plci, NULL, &pl));
   VkShaderModuleCreateInfo smci = { VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
      .codeSize = sizeof(draw_fetch_vert), .pCode = draw_fetch_vert };
   VkShaderModule vs;
   CHECK(vkCreateShaderModule(dev, &smci, NULL, &vs));

   VkVertexInputBindingDescription vb[3] = {
      { 0, 12, VK_VERTEX_INPUT_RATE_VERTEX },
      { 1, 4, VK_VERTEX_INPUT_RATE_INSTANCE },
      { 2, 0, VK_VERTEX_INPUT_RATE_INSTANCE },
   };
   VkVertexInputAttributeDescription va[3] = {
      { 0, 0, VK_FORMAT_R32G32B32_UINT, 0 },
      { 1, 1, VK_FORMAT_R32_UINT, 0 },
      { 2, 2, VK_FORMAT_R32G32B32A32_UINT, 0 },
   };
   VkVertexInputBindingDivisorDescriptionKHR div[2] = { { 1, 1 }, { 2, 0 } };
   VkPipelineVertexInputDivisorStateCreateInfoKHR divs = {
      VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_DIVISOR_STATE_CREATE_INFO_KHR,
      .vertexBindingDivisorCount = 2, .pVertexBindingDivisors = div };
   VkPipelineVertexInputStateCreateInfo vis = { VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO, &divs,
      .vertexBindingDescriptionCount = 3, .pVertexBindingDescriptions = vb,
      .vertexAttributeDescriptionCount = 3, .pVertexAttributeDescriptions = va };
   VkPipelineInputAssemblyStateCreateInfo ias = { VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
      .topology = VK_PRIMITIVE_TOPOLOGY_POINT_LIST };
   VkViewport vp = { 0, 0, 16, 16, 0, 1 };
   VkRect2D sc = { { 0, 0 }, { 16, 16 } };
   VkPipelineViewportStateCreateInfo vps = { VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO,
      .viewportCount = 1, .pViewports = &vp, .scissorCount = 1, .pScissors = &sc };
   VkPipelineRasterizationStateCreateInfo rs = { VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO,
      .rasterizerDiscardEnable = VK_TRUE, .lineWidth = 1.0f };
   VkDynamicState dyn[] = { VK_DYNAMIC_STATE_VERTEX_INPUT_BINDING_STRIDE };
   VkPipelineDynamicStateCreateInfo dys = { VK_STRUCTURE_TYPE_PIPELINE_DYNAMIC_STATE_CREATE_INFO,
      .dynamicStateCount = 1, .pDynamicStates = dyn };
   VkPipelineRenderingCreateInfo prci = { VK_STRUCTURE_TYPE_PIPELINE_RENDERING_CREATE_INFO };
   VkPipeline pipe[2];
   for (uint32_t i = 0; i < 2; i++) {
      uint32_t read_bases = i == 0;
      VkSpecializationMapEntry me = { 0, 0, 4 };
      VkSpecializationInfo spi = { 1, &me, 4, &read_bases };
      VkPipelineShaderStageCreateInfo st = { VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
         .stage = VK_SHADER_STAGE_VERTEX_BIT, .module = vs, .pName = "main", .pSpecializationInfo = &spi };
      VkGraphicsPipelineCreateInfo gpci = { VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO, &prci,
         .stageCount = 1, .pStages = &st, .pVertexInputState = &vis, .pInputAssemblyState = &ias,
         .pViewportState = &vps, .pRasterizationState = &rs, .pDynamicState = &dys, .layout = pl };
      CHECK(vkCreateGraphicsPipelines(dev, VK_NULL_HANDLE, 1, &gpci, NULL, &pipe[i]));
   }

   VkImage img_a = make_img(), img_b = make_img();

   VkCommandPoolCreateInfo cpi = { VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .queueFamilyIndex = qf };
   VkCommandPool pool;
   CHECK(vkCreateCommandPool(dev, &cpi, NULL, &pool));
   VkCommandBufferAllocateInfo cbai = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
      .commandPool = pool, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1 };
   VkCommandBuffer cb;
   CHECK(vkAllocateCommandBuffers(dev, &cbai, &cb));
   VkCommandBufferBeginInfo cbbi = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
      .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT };
   CHECK(vkBeginCommandBuffer(cb, &cbbi));

   VkImageMemoryBarrier imb[2];
   for (int i = 0; i < 2; i++) {
      imb[i] = (VkImageMemoryBarrier){ VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
         .dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT | VK_ACCESS_TRANSFER_WRITE_BIT,
         .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED, .newLayout = VK_IMAGE_LAYOUT_GENERAL,
         .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
         .image = i ? img_b : img_a, .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
   }
   vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT,
                        0, 0, NULL, 0, NULL, 2, imb);

   VkRenderingInfo ri = { VK_STRUCTURE_TYPE_RENDERING_INFO, .renderArea = { { 0, 0 }, { 16, 16 } },
      .layerCount = 1 };
   vkCmdBeginRendering(cb, &ri);
   vkCmdBindDescriptorSets(cb, VK_PIPELINE_BIND_POINT_GRAPHICS, pl, 0, 1, &ds, 0, NULL);
   vkCmdBindIndexBuffer(cb, idx_buf, 0, VK_INDEX_TYPE_UINT32);

#define BIND(vo, io) do { \
      VkBuffer bufs_[3] = { pos_buf, inst_buf, VK_NULL_HANDLE }; \
      VkDeviceSize offs_[3] = { (vo) * 12ull, (io) * 4ull, 0 }; \
      VkDeviceSize strides_[3] = { 12, 4, 0 }; \
      vkCmdBindVertexBuffers2(cb, 0, 3, bufs_, offs_, NULL, strides_); \
      cur_vo = (vo); cur_io = (io); } while (0)
#define TAG(name, p, n, ic) do { \
      struct tag_info *t_ = &tags[ntags]; \
      t_->what = name; t_->voff = cur_vo; t_->ioff = cur_io; t_->reads_bases = (p) == 0; \
      t_->expected = (n) * (ic); t_->inst_count = (ic); \
      vkCmdBindPipeline(cb, VK_PIPELINE_BIND_POINT_GRAPHICS, pipe[p]); \
      uint32_t pc_[4] = { ntags, cur_vo, cur_io, 0 }; \
      vkCmdPushConstants(cb, pl, VK_SHADER_STAGE_VERTEX_BIT, 0, 16, pc_); \
      ntags++; } while (0)
#define PAIR(b_i, b_v) do { struct tag_info *t_ = &tags[ntags - 1]; \
      t_->bi[t_->npairs] = (b_i); t_->bv[t_->npairs] = (b_v); t_->npairs++; } while (0)

   uint32_t cur_vo = 0, cur_io = 0;
   BIND(0, 0);

   TAG("draw 8v x3 first_vertex 5 first_instance 7", 0, 8, 3); PAIR(7, 5);
   vkCmdDraw(cb, 8, 3, 5, 7);
   TAG("indexed 16 x2 first_index 4 vertex_offset 100 first_instance 50", 0, 16, 2); PAIR(50, 100);
   vkCmdDrawIndexed(cb, 16, 2, 4, 100, 50);
   TAG("indexed 16 x2 vo 200 fi 300, shader without bases", 1, 16, 2); PAIR(300, 200);
   vkCmdDrawIndexed(cb, 16, 2, 0, 200, 300);
   TAG("same draw, shader with bases", 0, 16, 2); PAIR(300, 200);
   vkCmdDrawIndexed(cb, 16, 2, 0, 200, 300);
   TAG("indexed 16 x1 bases 0", 0, 16, 1); PAIR(0, 0);
   vkCmdDrawIndexed(cb, 16, 1, 0, 0, 0);
   BIND(10, 20);
   TAG("rebind at offsets, indexed 8 x2 vo 3 fi 9", 0, 8, 2); PAIR(9, 3);
   vkCmdDrawIndexed(cb, 8, 2, 0, 3, 9);
   BIND(10, 20);
   TAG("same binding again, draw 4 x1", 0, 4, 1); PAIR(0, 0);
   vkCmdDraw(cb, 4, 1, 0, 0);

   VkDrawIndexedIndirectCommand *ind = m_ind;
   for (uint32_t i = 0; i < 4; i++)
      ind[i] = (VkDrawIndexedIndirectCommand){ 8, 2, i * 8, (int32_t)(i + 1), 11 * (i + 1) };
   BIND(0, 0);
   TAG("indexed indirect x4 records", 0, 8 * 4, 2);
   for (uint32_t i = 0; i < 4; i++) PAIR(11 * (i + 1), i + 1);
   vkCmdDrawIndexedIndirect(cb, ind_buf, 0, 4, sizeof(*ind));
   TAG("indexed indirect x4 records, shader without bases", 1, 8 * 4, 2);
   for (uint32_t i = 0; i < 4; i++) PAIR(11 * (i + 1), i + 1);
   vkCmdDrawIndexedIndirect(cb, ind_buf, 0, 4, sizeof(*ind));

   VkDrawIndexedIndirectCommand *ind2 = (void *)((char *)m_ind + 1024);
   for (uint32_t i = 0; i < 4; i++)
      ind2[i] = (VkDrawIndexedIndirectCommand){ 4, 3, 0, (int32_t)(40 + i), 60 + i };
   *(uint32_t *)((char *)m_ind + 2048) = 3;
   TAG("indexed indirect count (3 of 4)", 0, 4 * 3, 3);
   for (uint32_t i = 0; i < 3; i++) PAIR(60 + i, 40 + i);
   vkCmdDrawIndexedIndirectCount(cb, ind_buf, 1024, ind_buf, 2048, 4, sizeof(*ind2));
   vkCmdEndRendering(cb);

   /* a meta blit and a render pass break; vertex buffers stay bound */
   VkImageBlit blit = { { VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1 }, { { 0, 0, 0 }, { 16, 16, 1 } },
                        { VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1 }, { { 0, 0, 0 }, { 8, 8, 1 } } };
   vkCmdBlitImage(cb, img_a, VK_IMAGE_LAYOUT_GENERAL, img_b, VK_IMAGE_LAYOUT_GENERAL, 1, &blit,
                  VK_FILTER_LINEAR);
   VkMemoryBarrier mb = { VK_STRUCTURE_TYPE_MEMORY_BARRIER, .srcAccessMask = VK_ACCESS_MEMORY_WRITE_BIT,
      .dstAccessMask = VK_ACCESS_MEMORY_READ_BIT | VK_ACCESS_MEMORY_WRITE_BIT };
   vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT,
                        0, 1, &mb, 0, NULL, 0, NULL);
   vkCmdBeginRendering(cb, &ri);
   vkCmdBindDescriptorSets(cb, VK_PIPELINE_BIND_POINT_GRAPHICS, pl, 0, 1, &ds, 0, NULL);
   TAG("after blit, no rebind, draw 4 x2 fi 5", 0, 4, 2); PAIR(5, 0);
   vkCmdDraw(cb, 4, 2, 0, 5);
   TAG("after blit, indexed 8 x2 vo 17 fi 23", 0, 8, 2); PAIR(23, 17);
   vkCmdDrawIndexed(cb, 8, 2, 0, 17, 23);
   vkCmdEndRendering(cb);

   VkMemoryBarrier host = { VK_STRUCTURE_TYPE_MEMORY_BARRIER, .srcAccessMask = VK_ACCESS_SHADER_WRITE_BIT,
      .dstAccessMask = VK_ACCESS_HOST_READ_BIT };
   vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT, VK_PIPELINE_STAGE_HOST_BIT,
                        0, 1, &host, 0, NULL, 0, NULL);
   CHECK(vkEndCommandBuffer(cb));

   VkFenceCreateInfo fci = { VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
   VkFence fence;
   CHECK(vkCreateFence(dev, &fci, NULL, &fence));
   VkSubmitInfo si = { VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cb };
   CHECK(vkQueueSubmit(q, 1, &si, fence));
   CHECK(vkWaitForFences(dev, 1, &fence, VK_TRUE, 5ull * 1000 * 1000 * 1000));

   const uint32_t *o = m_out;
   uint32_t count = o[0], shown = 0, bad_total = 0, total_expected = 0;
   const uint32_t *rec = o + 4;
   for (uint32_t s = 0; s < count && s < MAXREC; s++) {
      const uint32_t *r = rec + s * 12;
      uint32_t tag = r[0], vi = r[1], ii = r[2], bi = r[3];
      uint32_t px = r[4], py = r[5], ia = r[6], bv = r[7];
      const char *why = NULL;
      if (tag >= ntags) {
         why = "bad tag";
      } else {
         struct tag_info *t = &tags[tag];
         t->seen++;
         if (px != vi + t->voff) why = "b0 fetch != gl_VertexIndex + offset";
         else if (py != (px ^ 0x5a5a5a5au)) why = "b0 fetch inconsistent";
         else if (ia != ii + t->ioff + 0x10000u) why = "b1 per-instance fetch != gl_InstanceIndex + offset";
         else if (r[8] | r[9] | r[10] | r[11]) why = "null b2 fetch not zero";
         else if (t->reads_bases) {
            int ok = 0;
            for (uint32_t k = 0; k < t->npairs; k++)
               if (bi == t->bi[k] && bv == t->bv[k] && ii - bi < t->inst_count)
                  ok = 1;
            if (!ok) why = "gl_BaseInstance/gl_BaseVertex/instance range wrong";
         } else {
            int ok = 0;
            for (uint32_t k = 0; k < t->npairs; k++)
               if (ii - t->bi[k] < t->inst_count)
                  ok = 1;
            if (!ok) why = "gl_InstanceIndex outside the draw's instances";
         }
         if (why) t->bad++;
      }
      if (why) {
         bad_total++;
         if (shown++ < 24)
            printf("  BAD rec %u tag %u: %s (vi %u ii %u bi %u bv %u b0 %u/%08x b1 %08x b2 %08x %08x %08x %08x)\n",
                   s, tag, why, vi, ii, bi, bv, px, py, ia, r[8], r[9], r[10], r[11]);
      }
   }
   for (uint32_t t = 0; t < ntags; t++) {
      total_expected += tags[t].expected;
      int ok = tags[t].seen == tags[t].expected && !tags[t].bad;
      if (!ok) bad_total++;
      printf("tag %2u %-4s seen %4u/%-4u bad %4u  %s\n", t, ok ? "ok" : "FAIL", tags[t].seen,
             tags[t].expected, tags[t].bad, tags[t].what);
   }
   printf("RESULT %s records=%u expected=%u bad=%u\n", bad_total ? "FAIL" : "PASS", count,
          total_expected, bad_total);
   vkDeviceWaitIdle(dev);
   vkDestroyDevice(dev, NULL);
   vkDestroyInstance(inst, NULL);
   return bad_total ? 2 : 0;
}
