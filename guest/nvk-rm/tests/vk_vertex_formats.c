/* vk_vertex_formats: vertex attribute format conversion, hashed so that a run
 * on another driver for the same GPU can be compared (vertex fetch converts
 * in hardware: the results must be bit-identical).  16 attributes in two
 * bindings (strides 64 and 44, the formats D3D11 input layouts use), 64
 * points with rasterizer discard.
 *
 *   vk_vertex_formats [device-substring=NVK]
 */
#ifdef _WIN32
#include <windows.h>
#endif
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>

#include "vertex_formats_vert.h"

#define CHECK(x) do { VkResult r_ = (x); if (r_ != VK_SUCCESS) { \
   fprintf(stderr, "FAIL %s:%d: %s -> %d\n", __FILE__, __LINE__, #x, r_); exit(1); } } while (0)


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


   static const struct { uint32_t loc, binding; VkFormat f; uint32_t off; const char *name; } at[16] = {
      { 0, 0, VK_FORMAT_R16G16_SFLOAT, 0, "R16G16_SFLOAT" },
      { 1, 0, VK_FORMAT_R16G16_SNORM, 4, "R16G16_SNORM" },
      { 2, 0, VK_FORMAT_R16G16_UNORM, 8, "R16G16_UNORM" },
      { 3, 0, VK_FORMAT_R16G16B16A16_SNORM, 12, "R16G16B16A16_SNORM" },
      { 4, 0, VK_FORMAT_R16G16B16A16_SFLOAT, 20, "R16G16B16A16_SFLOAT" },
      { 5, 0, VK_FORMAT_R8G8B8A8_UNORM, 28, "R8G8B8A8_UNORM" },
      { 6, 0, VK_FORMAT_R8G8B8A8_SNORM, 32, "R8G8B8A8_SNORM" },
      { 7, 0, VK_FORMAT_A2B10G10R10_UNORM_PACK32, 36, "A2B10G10R10_UNORM" },
      { 8, 0, VK_FORMAT_R32G32_SFLOAT, 40, "R32G32_SFLOAT" },
      { 9, 0, VK_FORMAT_R32G32B32_SFLOAT, 48, "R32G32B32_SFLOAT" },
      { 10, 0, VK_FORMAT_B8G8R8A8_UNORM, 60, "B8G8R8A8_UNORM" },
      { 11, 1, VK_FORMAT_R16_SNORM, 4, "R16_SNORM" },
      { 12, 1, VK_FORMAT_R8G8B8A8_UINT, 0, "R8G8B8A8_UINT" },
      { 13, 1, VK_FORMAT_R16G16_SINT, 8, "R16G16_SINT" },
      { 14, 1, VK_FORMAT_R32G32B32A32_UINT, 12, "R32G32B32A32_UINT" },
      { 15, 1, VK_FORMAT_R16G16B16A16_SINT, 28, "R16G16B16A16_SINT" },
   };
   VkVertexInputAttributeDescription va[16];
   uint32_t nva = 0;
   for (int i = 0; i < 16; i++) {
      VkFormatProperties fp;
      vkGetPhysicalDeviceFormatProperties(pd, at[i].f, &fp);
      if (!(fp.bufferFeatures & VK_FORMAT_FEATURE_VERTEX_BUFFER_BIT)) {
         printf("format %s: no vertex buffer support, skipped\n", at[i].name);
         continue;
      }
      va[nva++] = (VkVertexInputAttributeDescription){ at[i].loc, at[i].binding, at[i].f, at[i].off };
   }

   void *m_out, *m_vb0, *m_vb1;
   VkBuffer out_buf = make_buf(64 * 16 * 16, VK_BUFFER_USAGE_STORAGE_BUFFER_BIT, &m_out);
   VkBuffer vb0 = make_buf(64 * 64, VK_BUFFER_USAGE_VERTEX_BUFFER_BIT, &m_vb0);
   VkBuffer vb1 = make_buf(64 * 44, VK_BUFFER_USAGE_VERTEX_BUFFER_BIT, &m_vb1);
   memset(m_out, 0xcd, 64 * 16 * 16);
   uint32_t rng = 0x9e3779b9u;
   for (int i = 0; i < 64 * 64; i++) { rng ^= rng << 13; rng ^= rng >> 17; rng ^= rng << 5; ((uint8_t *)m_vb0)[i] = rng; }
   for (int i = 0; i < 64 * 44; i++) { rng ^= rng << 13; rng ^= rng >> 17; rng ^= rng << 5; ((uint8_t *)m_vb1)[i] = rng; }
   /* keep some finite, typical values in the first 8 vertices */
   for (int v = 0; v < 8; v++) {
      float *f = (float *)((char *)m_vb0 + v * 64 + 40);
      f[0] = v * 0.25f; f[1] = -v * 1.5f; f[2] = 3.0f; f[3] = 1.0f / (v + 1); f[4] = 1e-3f * v;
   }

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
   VkPipelineLayoutCreateInfo plci = { VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
      .setLayoutCount = 1, .pSetLayouts = &dsl };
   VkPipelineLayout pl;
   CHECK(vkCreatePipelineLayout(dev, &plci, NULL, &pl));
   VkShaderModuleCreateInfo smci = { VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
      .codeSize = sizeof(vertex_formats_vert), .pCode = vertex_formats_vert };
   VkShaderModule vs;
   CHECK(vkCreateShaderModule(dev, &smci, NULL, &vs));
   VkVertexInputBindingDescription vb[2] = { { 0, 64, VK_VERTEX_INPUT_RATE_VERTEX }, { 1, 44, VK_VERTEX_INPUT_RATE_VERTEX } };
   VkPipelineVertexInputStateCreateInfo vis = { VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO,
      .vertexBindingDescriptionCount = 2, .pVertexBindingDescriptions = vb,
      .vertexAttributeDescriptionCount = nva, .pVertexAttributeDescriptions = va };
   VkPipelineInputAssemblyStateCreateInfo ias = { VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
      .topology = VK_PRIMITIVE_TOPOLOGY_POINT_LIST };
   VkViewport vp = { 0, 0, 1, 1, 0, 1 };
   VkRect2D sc = { { 0, 0 }, { 1, 1 } };
   VkPipelineViewportStateCreateInfo vps = { VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO,
      .viewportCount = 1, .pViewports = &vp, .scissorCount = 1, .pScissors = &sc };
   VkPipelineRasterizationStateCreateInfo rs = { VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO,
      .rasterizerDiscardEnable = VK_TRUE, .lineWidth = 1.0f };
   VkPipelineRenderingCreateInfo prci = { VK_STRUCTURE_TYPE_PIPELINE_RENDERING_CREATE_INFO };
   VkPipelineShaderStageCreateInfo st = { VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
      .stage = VK_SHADER_STAGE_VERTEX_BIT, .module = vs, .pName = "main" };
   VkGraphicsPipelineCreateInfo gpci = { VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO, &prci,
      .stageCount = 1, .pStages = &st, .pVertexInputState = &vis, .pInputAssemblyState = &ias,
      .pViewportState = &vps, .pRasterizationState = &rs, .layout = pl };
   VkPipeline pipe;
   CHECK(vkCreateGraphicsPipelines(dev, VK_NULL_HANDLE, 1, &gpci, NULL, &pipe));

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
   VkRenderingInfo ri = { VK_STRUCTURE_TYPE_RENDERING_INFO, .renderArea = { { 0, 0 }, { 1, 1 } }, .layerCount = 1 };
   vkCmdBeginRendering(cb, &ri);
   vkCmdBindPipeline(cb, VK_PIPELINE_BIND_POINT_GRAPHICS, pipe);
   vkCmdBindDescriptorSets(cb, VK_PIPELINE_BIND_POINT_GRAPHICS, pl, 0, 1, &ds, 0, NULL);
   VkBuffer bufs[2] = { vb0, vb1 };
   VkDeviceSize offs[2] = { 0, 0 };
   vkCmdBindVertexBuffers(cb, 0, 2, bufs, offs);
   vkCmdDraw(cb, 64, 1, 0, 0);
   vkCmdEndRendering(cb);
   VkMemoryBarrier host = { VK_STRUCTURE_TYPE_MEMORY_BARRIER, .srcAccessMask = VK_ACCESS_SHADER_WRITE_BIT,
      .dstAccessMask = VK_ACCESS_HOST_READ_BIT };
   vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT, VK_PIPELINE_STAGE_HOST_BIT, 0, 1, &host, 0, NULL, 0, NULL);
   CHECK(vkEndCommandBuffer(cb));
   VkFenceCreateInfo fci = { VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
   VkFence fence;
   CHECK(vkCreateFence(dev, &fci, NULL, &fence));
   VkSubmitInfo si = { VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cb };
   CHECK(vkQueueSubmit(q, 1, &si, fence));
   CHECK(vkWaitForFences(dev, 1, &fence, VK_TRUE, 5ull * 1000 * 1000 * 1000));

   const uint32_t *o = m_out;
   uint64_t all = 0xcbf29ce484222325ull;
   for (int i = 0; i < 16; i++) {
      uint64_t h = 0xcbf29ce484222325ull;
      for (int v = 0; v < 64; v++)
         for (int c = 0; c < 4; c++) {
            uint32_t x = o[(v * 16 + at[i].loc) * 4 + c];
            for (int k = 0; k < 4; k++) { h ^= (x >> (8 * k)) & 0xff; h *= 0x100000001b3ull; }
         }
      all ^= h + 0x9e3779b97f4a7c15ull + (all << 6) + (all >> 2);
      const uint32_t *v1 = &o[(1 * 16 + at[i].loc) * 4];
      printf("%-20s hash %016llx  v1 %08x %08x %08x %08x\n", at[i].name, (unsigned long long)h,
             v1[0], v1[1], v1[2], v1[3]);
   }
   printf("RESULT all_hash=%016llx\n", (unsigned long long)all);
   vkDeviceWaitIdle(dev);
   vkDestroyDevice(dev, NULL);
   vkDestroyInstance(inst, NULL);
   return 0;
}
