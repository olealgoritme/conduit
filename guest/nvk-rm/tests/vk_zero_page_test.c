/* vk_zero_page_test: does the memory behind null vertex buffers stay zero?
 *
 * Draws 256 points with rasterizer discard and two null vertex buffers
 * (robustness2 nullDescriptor): binding 0 per vertex with stride 16, binding 1
 * per instance with stride 0 and divisor 0 (what DXVK binds for an unused
 * D3D11 input slot).  The vertex shader copies the fetched RGBA32UI values
 * to a storage buffer, so binding 0 dumps the first 4 KiB the null binding
 * reads.  Phase 0 fetches on a fresh device; phases 1..7 first run one
 * compute dispatch that writes through a null or out-of-bounds descriptor
 * (each of which must discard the write), then fetch again.
 *
 * GPU time: a few microseconds.  Prints one line per phase and a RESULT line.
 */
#ifdef _WIN32
#include <windows.h>
#endif
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>

#include "zero_page_fetch_vert.h"
#include "zero_page_write_comp.h"

#define CHECK(x) do { VkResult r_ = (x); if (r_ != VK_SUCCESS) { \
   fprintf(stderr, "FAIL %s:%d: %s -> %d\n", __FILE__, __LINE__, #x, r_); exit(1); } } while (0)

#define NPHASE 8
#define SLOTS 260

static const char *mode_name[NPHASE] = {
   "fresh device",
   "imageStore null storage image",
   "imageAtomicAdd null storage image",
   "imageStore null storage texel buffer",
   "imageStore OOB storage texel buffer",
   "store null SSBO",
   "imageStore null rgba8 2D array image",
   "imageStore OOB storage image",
};

static VkPhysicalDeviceMemoryProperties memprops;

static uint32_t find_mem(uint32_t bits, VkMemoryPropertyFlags want)
{
   for (uint32_t i = 0; i < memprops.memoryTypeCount; i++)
      if ((bits & (1u << i)) && (memprops.memoryTypes[i].propertyFlags & want) == want)
         return i;
   fprintf(stderr, "FAIL no memory type\n");
   exit(1);
}

static int has_ext(const VkExtensionProperties *e, uint32_t n, const char *name)
{
   for (uint32_t i = 0; i < n; i++)
      if (!strcmp(e[i].extensionName, name))
         return 1;
   return 0;
}

int main(int argc, char **argv)
{
   const char *want = argc > 1 ? argv[1] : "NVK";

   VkApplicationInfo app = { VK_STRUCTURE_TYPE_APPLICATION_INFO, .apiVersion = VK_API_VERSION_1_3 };
   VkInstanceCreateInfo ici = { VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app };
   VkInstance inst;
   CHECK(vkCreateInstance(&ici, NULL, &inst));

   uint32_t npd = 8;
   VkPhysicalDevice pds[8];
   CHECK(vkEnumeratePhysicalDevices(inst, &npd, pds));
   VkPhysicalDevice pd = VK_NULL_HANDLE;
   VkPhysicalDeviceProperties props;
   for (uint32_t i = 0; i < npd; i++) {
      vkGetPhysicalDeviceProperties(pds[i], &props);
      printf("device %u: %s\n", i, props.deviceName);
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
   const char *robust = has_ext(exts, next, "VK_KHR_robustness2") ? "VK_KHR_robustness2" : "VK_EXT_robustness2";
   const char *divisor = has_ext(exts, next, "VK_KHR_vertex_attribute_divisor") ?
                         "VK_KHR_vertex_attribute_divisor" : "VK_EXT_vertex_attribute_divisor";
   const char *dev_exts[] = { robust, divisor };

   uint32_t qf = 0, nqf = 16;
   VkQueueFamilyProperties qfp[16];
   vkGetPhysicalDeviceQueueFamilyProperties(pd, &nqf, qfp);
   for (qf = 0; qf < nqf; qf++)
      if (qfp[qf].queueFlags & VK_QUEUE_GRAPHICS_BIT)
         break;

   VkPhysicalDeviceVertexAttributeDivisorFeaturesKHR divf = {
      VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VERTEX_ATTRIBUTE_DIVISOR_FEATURES_KHR,
      .vertexAttributeInstanceRateDivisor = VK_TRUE,
      .vertexAttributeInstanceRateZeroDivisor = VK_TRUE,
   };
   VkPhysicalDeviceRobustness2FeaturesEXT r2 = {
      VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ROBUSTNESS_2_FEATURES_EXT, &divf,
      .robustBufferAccess2 = VK_TRUE, .robustImageAccess2 = VK_TRUE, .nullDescriptor = VK_TRUE,
   };
   VkPhysicalDeviceVulkan13Features f13 = {
      VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES, &r2,
      .dynamicRendering = VK_TRUE, .robustImageAccess = VK_TRUE,
   };
   VkPhysicalDeviceFeatures2 f2 = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2, &f13 };
   f2.features.robustBufferAccess = VK_TRUE;
   f2.features.vertexPipelineStoresAndAtomics = VK_TRUE;
   f2.features.shaderStorageImageWriteWithoutFormat = VK_TRUE;

   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = { VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = qf, .queueCount = 1, .pQueuePriorities = &prio };
   VkDeviceCreateInfo dci = { VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, &f2,
      .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci,
      .enabledExtensionCount = 2, .ppEnabledExtensionNames = dev_exts };
   VkDevice dev;
   CHECK(vkCreateDevice(pd, &dci, NULL, &dev));
   VkQueue q;
#ifdef _WIN32
   {
      char path[MAX_PATH] = "";
      HMODULE m = GetModuleHandleA("vulkan_nouveau.dll");
      if (m)
         GetModuleFileNameA(m, path, sizeof(path));
      printf("icd: %s\n", m ? path : "(vulkan_nouveau.dll not loaded)");
   }
#endif
   vkGetDeviceQueue(dev, qf, 0, &q);

   /* Output buffer (host visible) and a 4-byte texel buffer for OOB stores */
   const VkDeviceSize out_size = (VkDeviceSize)NPHASE * SLOTS * 16;
   VkBuffer out_buf, small_buf;
   VkDeviceMemory out_mem, small_mem;
   {
      VkBufferCreateInfo bci = { VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size = out_size,
         .usage = VK_BUFFER_USAGE_STORAGE_BUFFER_BIT };
      CHECK(vkCreateBuffer(dev, &bci, NULL, &out_buf));
      VkMemoryRequirements mr;
      vkGetBufferMemoryRequirements(dev, out_buf, &mr);
      VkMemoryAllocateInfo mai = { VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = mr.size,
         .memoryTypeIndex = find_mem(mr.memoryTypeBits, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                                                        VK_MEMORY_PROPERTY_HOST_COHERENT_BIT) };
      CHECK(vkAllocateMemory(dev, &mai, NULL, &out_mem));
      CHECK(vkBindBufferMemory(dev, out_buf, out_mem, 0));

      bci.size = 256;
      bci.usage = VK_BUFFER_USAGE_STORAGE_TEXEL_BUFFER_BIT;
      CHECK(vkCreateBuffer(dev, &bci, NULL, &small_buf));
      vkGetBufferMemoryRequirements(dev, small_buf, &mr);
      mai.allocationSize = mr.size;
      mai.memoryTypeIndex = find_mem(mr.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT);
      CHECK(vkAllocateMemory(dev, &mai, NULL, &small_mem));
      CHECK(vkBindBufferMemory(dev, small_buf, small_mem, 0));
   }
   void *out_map;
   CHECK(vkMapMemory(dev, out_mem, 0, out_size, 0, &out_map));
   memset(out_map, 0xcd, out_size);

   VkBufferView small_view;
   VkBufferViewCreateInfo bvci = { VK_STRUCTURE_TYPE_BUFFER_VIEW_CREATE_INFO, .buffer = small_buf,
      .format = VK_FORMAT_R32_UINT, .offset = 0, .range = 4 };
   CHECK(vkCreateBufferView(dev, &bvci, NULL, &small_view));

   /* 4x4 R32UI storage image for OOB image stores */
   VkImage small_img;
   VkDeviceMemory small_img_mem;
   VkImageView small_img_view;
   {
      VkImageCreateInfo ii = { VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO, .imageType = VK_IMAGE_TYPE_2D,
         .format = VK_FORMAT_R32_UINT, .extent = { 4, 4, 1 }, .mipLevels = 1, .arrayLayers = 1,
         .samples = VK_SAMPLE_COUNT_1_BIT, .tiling = VK_IMAGE_TILING_OPTIMAL,
         .usage = VK_IMAGE_USAGE_STORAGE_BIT, .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED };
      CHECK(vkCreateImage(dev, &ii, NULL, &small_img));
      VkMemoryRequirements mr;
      vkGetImageMemoryRequirements(dev, small_img, &mr);
      VkMemoryAllocateInfo mai = { VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = mr.size,
         .memoryTypeIndex = find_mem(mr.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT) };
      CHECK(vkAllocateMemory(dev, &mai, NULL, &small_img_mem));
      CHECK(vkBindImageMemory(dev, small_img, small_img_mem, 0));
      VkImageViewCreateInfo vi = { VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO, .image = small_img,
         .viewType = VK_IMAGE_VIEW_TYPE_2D, .format = VK_FORMAT_R32_UINT,
         .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
      CHECK(vkCreateImageView(dev, &vi, NULL, &small_img_view));
   }

   /* Descriptors: 0 out SSBO, 1 null storage image, 2 null storage texel
    * buffer, 3 4-byte texel buffer, 4 null SSBO, 5 null 2D array image,
    * 6 4x4 storage image */
   VkDescriptorSetLayoutBinding b[7] = {
      { 0, VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 1, VK_SHADER_STAGE_ALL },
      { 1, VK_DESCRIPTOR_TYPE_STORAGE_IMAGE, 1, VK_SHADER_STAGE_ALL },
      { 2, VK_DESCRIPTOR_TYPE_STORAGE_TEXEL_BUFFER, 1, VK_SHADER_STAGE_ALL },
      { 3, VK_DESCRIPTOR_TYPE_STORAGE_TEXEL_BUFFER, 1, VK_SHADER_STAGE_ALL },
      { 4, VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 1, VK_SHADER_STAGE_ALL },
      { 5, VK_DESCRIPTOR_TYPE_STORAGE_IMAGE, 1, VK_SHADER_STAGE_ALL },
      { 6, VK_DESCRIPTOR_TYPE_STORAGE_IMAGE, 1, VK_SHADER_STAGE_ALL },
   };
   VkDescriptorSetLayoutCreateInfo dslci = { VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
      .bindingCount = 7, .pBindings = b };
   VkDescriptorSetLayout dsl;
   CHECK(vkCreateDescriptorSetLayout(dev, &dslci, NULL, &dsl));
   VkDescriptorPoolSize ps[3] = {
      { VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 2 },
      { VK_DESCRIPTOR_TYPE_STORAGE_IMAGE, 3 },
      { VK_DESCRIPTOR_TYPE_STORAGE_TEXEL_BUFFER, 2 },
   };
   VkDescriptorPoolCreateInfo dpci = { VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
      .maxSets = 1, .poolSizeCount = 3, .pPoolSizes = ps };
   VkDescriptorPool dp;
   CHECK(vkCreateDescriptorPool(dev, &dpci, NULL, &dp));
   VkDescriptorSetAllocateInfo dsai = { VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
      .descriptorPool = dp, .descriptorSetCount = 1, .pSetLayouts = &dsl };
   VkDescriptorSet ds;
   CHECK(vkAllocateDescriptorSets(dev, &dsai, &ds));

   VkDescriptorBufferInfo out_bi = { out_buf, 0, VK_WHOLE_SIZE };
   VkDescriptorBufferInfo null_bi = { VK_NULL_HANDLE, 0, VK_WHOLE_SIZE };
   VkDescriptorImageInfo null_ii = { VK_NULL_HANDLE, VK_NULL_HANDLE, VK_IMAGE_LAYOUT_GENERAL };
   VkDescriptorImageInfo small_ii = { VK_NULL_HANDLE, small_img_view, VK_IMAGE_LAYOUT_GENERAL };
   VkBufferView null_bv = VK_NULL_HANDLE;
   VkWriteDescriptorSet w[7] = {
      { VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = ds, .dstBinding = 0, .descriptorCount = 1,
        .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, .pBufferInfo = &out_bi },
      { VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = ds, .dstBinding = 1, .descriptorCount = 1,
        .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_IMAGE, .pImageInfo = &null_ii },
      { VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = ds, .dstBinding = 2, .descriptorCount = 1,
        .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_TEXEL_BUFFER, .pTexelBufferView = &null_bv },
      { VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = ds, .dstBinding = 3, .descriptorCount = 1,
        .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_TEXEL_BUFFER, .pTexelBufferView = &small_view },
      { VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = ds, .dstBinding = 4, .descriptorCount = 1,
        .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, .pBufferInfo = &null_bi },
      { VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = ds, .dstBinding = 5, .descriptorCount = 1,
        .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_IMAGE, .pImageInfo = &null_ii },
      { VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = ds, .dstBinding = 6, .descriptorCount = 1,
        .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_IMAGE, .pImageInfo = &small_ii },
   };
   vkUpdateDescriptorSets(dev, 7, w, 0, NULL);

   VkPushConstantRange pcr = { VK_SHADER_STAGE_ALL, 0, 8 };
   VkPipelineLayoutCreateInfo plci = { VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
      .setLayoutCount = 1, .pSetLayouts = &dsl, .pushConstantRangeCount = 1, .pPushConstantRanges = &pcr };
   VkPipelineLayout pl;
   CHECK(vkCreatePipelineLayout(dev, &plci, NULL, &pl));

   VkShaderModule vs, cs;
   VkShaderModuleCreateInfo smci = { VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
      .codeSize = sizeof(zero_page_fetch_vert), .pCode = zero_page_fetch_vert };
   CHECK(vkCreateShaderModule(dev, &smci, NULL, &vs));
   smci.codeSize = sizeof(zero_page_write_comp);
   smci.pCode = zero_page_write_comp;
   CHECK(vkCreateShaderModule(dev, &smci, NULL, &cs));

   VkComputePipelineCreateInfo cpci = { VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO,
      .stage = { VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, .stage = VK_SHADER_STAGE_COMPUTE_BIT,
                 .module = cs, .pName = "main" },
      .layout = pl };
   VkPipeline cpipe;
   CHECK(vkCreateComputePipelines(dev, VK_NULL_HANDLE, 1, &cpci, NULL, &cpipe));

   VkVertexInputBindingDescription vb[2] = {
      { 0, 16, VK_VERTEX_INPUT_RATE_VERTEX },
      { 1, 0, VK_VERTEX_INPUT_RATE_INSTANCE },
   };
   VkVertexInputAttributeDescription va[3] = {
      { 0, 0, VK_FORMAT_R32G32B32A32_UINT, 0 },
      { 1, 1, VK_FORMAT_R32G32B32A32_UINT, 0 },
      { 2, 1, VK_FORMAT_R32G32B32A32_UINT, 2032 },
   };
   VkVertexInputBindingDivisorDescriptionKHR div = { 1, 0 };
   VkPipelineVertexInputDivisorStateCreateInfoKHR divs = {
      VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_DIVISOR_STATE_CREATE_INFO_KHR,
      .vertexBindingDivisorCount = 1, .pVertexBindingDivisors = &div };
   VkPipelineVertexInputStateCreateInfo vis = { VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO, &divs,
      .vertexBindingDescriptionCount = 2, .pVertexBindingDescriptions = vb,
      .vertexAttributeDescriptionCount = 3, .pVertexAttributeDescriptions = va };
   VkPipelineInputAssemblyStateCreateInfo ias = { VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
      .topology = VK_PRIMITIVE_TOPOLOGY_POINT_LIST };
   VkViewport vp = { 0, 0, 1, 1, 0, 1 };
   VkRect2D sc = { { 0, 0 }, { 1, 1 } };
   VkPipelineViewportStateCreateInfo vps = { VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO,
      .viewportCount = 1, .pViewports = &vp, .scissorCount = 1, .pScissors = &sc };
   VkPipelineRasterizationStateCreateInfo rs = { VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO,
      .rasterizerDiscardEnable = VK_TRUE, .lineWidth = 1.0f };
   VkPipelineRenderingCreateInfo prci = { VK_STRUCTURE_TYPE_PIPELINE_RENDERING_CREATE_INFO };
   VkPipelineShaderStageCreateInfo vstage = { VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
      .stage = VK_SHADER_STAGE_VERTEX_BIT, .module = vs, .pName = "main" };
   VkGraphicsPipelineCreateInfo gpci = { VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO, &prci,
      .stageCount = 1, .pStages = &vstage, .pVertexInputState = &vis, .pInputAssemblyState = &ias,
      .pViewportState = &vps, .pRasterizationState = &rs, .layout = pl };
   VkPipeline gpipe;
   CHECK(vkCreateGraphicsPipelines(dev, VK_NULL_HANDLE, 1, &gpci, NULL, &gpipe));

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

   VkMemoryBarrier full = { VK_STRUCTURE_TYPE_MEMORY_BARRIER,
      .srcAccessMask = VK_ACCESS_MEMORY_WRITE_BIT,
      .dstAccessMask = VK_ACCESS_MEMORY_READ_BIT | VK_ACCESS_MEMORY_WRITE_BIT };
   VkImageMemoryBarrier ib = { VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
      .dstAccessMask = VK_ACCESS_SHADER_WRITE_BIT, .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
      .newLayout = VK_IMAGE_LAYOUT_GENERAL, .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
      .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .image = small_img,
      .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
   vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT,
                        0, 0, NULL, 0, NULL, 1, &ib);

   VkRenderingInfo ri = { VK_STRUCTURE_TYPE_RENDERING_INFO, .renderArea = { { 0, 0 }, { 1, 1 } },
      .layerCount = 1 };
   VkBuffer null_bufs[2] = { VK_NULL_HANDLE, VK_NULL_HANDLE };
   VkDeviceSize offs[2] = { 0, 0 };

   for (uint32_t phase = 0; phase < NPHASE; phase++) {
      uint32_t pc[2] = { phase, phase };
      if (phase > 0) {
         vkCmdBindPipeline(cb, VK_PIPELINE_BIND_POINT_COMPUTE, cpipe);
         vkCmdBindDescriptorSets(cb, VK_PIPELINE_BIND_POINT_COMPUTE, pl, 0, 1, &ds, 0, NULL);
         vkCmdPushConstants(cb, pl, VK_SHADER_STAGE_ALL, 0, 8, pc);
         vkCmdDispatch(cb, 1, 1, 1);
         vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT,
                              0, 1, &full, 0, NULL, 0, NULL);
      }
      vkCmdBeginRendering(cb, &ri);
      vkCmdBindPipeline(cb, VK_PIPELINE_BIND_POINT_GRAPHICS, gpipe);
      vkCmdBindDescriptorSets(cb, VK_PIPELINE_BIND_POINT_GRAPHICS, pl, 0, 1, &ds, 0, NULL);
      vkCmdPushConstants(cb, pl, VK_SHADER_STAGE_ALL, 0, 8, pc);
      vkCmdBindVertexBuffers(cb, 0, 2, null_bufs, offs);
      vkCmdDraw(cb, 256, 1, 0, 0);
      vkCmdEndRendering(cb);
      vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT,
                           0, 1, &full, 0, NULL, 0, NULL);
   }
   VkMemoryBarrier host = { VK_STRUCTURE_TYPE_MEMORY_BARRIER,
      .srcAccessMask = VK_ACCESS_SHADER_WRITE_BIT, .dstAccessMask = VK_ACCESS_HOST_READ_BIT };
   vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT, VK_PIPELINE_STAGE_HOST_BIT,
                        0, 1, &host, 0, NULL, 0, NULL);
   CHECK(vkEndCommandBuffer(cb));

   VkFenceCreateInfo fci = { VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
   VkFence fence;
   CHECK(vkCreateFence(dev, &fci, NULL, &fence));
   VkSubmitInfo si = { VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cb };
   CHECK(vkQueueSubmit(q, 1, &si, fence));
   CHECK(vkWaitForFences(dev, 1, &fence, VK_TRUE, 5ull * 1000 * 1000 * 1000));

   const uint32_t *o = out_map;
   int bad_phases = 0, first_bad = -1;
   for (uint32_t phase = 0; phase < NPHASE; phase++) {
      const uint32_t *p = o + phase * SLOTS * 4;
      uint32_t nz = 0, unwritten = 0, first = ~0u;
      for (uint32_t i = 0; i < 258 * 4; i++) {
         if (p[i] == 0xcdcdcdcdu)
            unwritten++;
         else if (p[i]) {
            nz++;
            if (first == ~0u)
               first = i;
         }
      }
      printf("phase %u (%s): nonzero=%u unwritten=%u inst=%08x %08x %08x %08x off2032=%08x %08x %08x %08x",
             phase, mode_name[phase], nz, unwritten,
             p[256 * 4], p[256 * 4 + 1], p[256 * 4 + 2], p[256 * 4 + 3],
             p[257 * 4], p[257 * 4 + 1], p[257 * 4 + 2], p[257 * 4 + 3]);
      if (first != ~0u)
         printf(" first_nonzero_byte=0x%x value=%08x", first * 4, p[first]);
      printf("\n");
      if (nz) {
         uint32_t shown = 0;
         for (uint32_t i = 0; i < 256 * 4 && shown < 48; i++)
            if (p[i] && p[i] != 0xcdcdcdcdu) {
               printf("   page+0x%03x: %08x\n", i * 4, p[i]);
               shown++;
            }
      }
      if (nz || unwritten) {
         bad_phases++;
         if (first_bad < 0)
            first_bad = phase;
      }
   }
   printf("RESULT %s bad_phases=%d first_bad=%d\n", bad_phases ? "FAIL" : "PASS", bad_phases, first_bad);

   vkDeviceWaitIdle(dev);
   vkDestroyDevice(dev, NULL);
   vkDestroyInstance(inst, NULL);
   return bad_phases ? 2 : 0;
}
