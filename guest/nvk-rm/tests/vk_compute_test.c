/* Minimal Vulkan compute test: one storage buffer, one dispatch that writes
 * data[i] = i * mul + 7, read back and verify.  Optionally (argv[2] = "copy")
 * also round-trips through a device-local buffer with vkCmdCopyBuffer.
 *
 *   vk_compute_test compute.spv [copy] [N]
 */
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>

#include "vk_direct_driver.h"

#define CHECK(x) do { VkResult r_ = (x); \
   if (r_ != VK_SUCCESS) { fprintf(stderr, "FAIL %s:%d: %s -> %d\n", __FILE__, __LINE__, #x, r_); exit(1); } \
   } while (0)
#define STEP(s) do { fprintf(stderr, "step: %s\n", s); } while (0)

static uint32_t *read_file(const char *path, size_t *size)
{
   FILE *f = fopen(path, "rb");
   if (!f) { perror(path); exit(1); }
   fseek(f, 0, SEEK_END);
   *size = ftell(f);
   fseek(f, 0, SEEK_SET);
   uint32_t *buf = malloc(*size);
   if (fread(buf, 1, *size, f) != *size) { perror("fread"); exit(1); }
   fclose(f);
   return buf;
}

static uint32_t find_mem(VkPhysicalDeviceMemoryProperties *mp, uint32_t bits, VkMemoryPropertyFlags want)
{
   for (uint32_t i = 0; i < mp->memoryTypeCount; i++)
      if ((bits & (1u << i)) && (mp->memoryTypes[i].propertyFlags & want) == want)
         return i;
   fprintf(stderr, "no memory type for bits 0x%x flags 0x%x\n", bits, want);
   exit(1);
}

static void make_buffer(VkDevice dev, VkPhysicalDeviceMemoryProperties *mp, VkDeviceSize size,
                        VkBufferUsageFlags usage, VkMemoryPropertyFlags flags,
                        VkBuffer *buf, VkDeviceMemory *mem)
{
   VkBufferCreateInfo bci = { .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
      .size = size, .usage = usage, .sharingMode = VK_SHARING_MODE_EXCLUSIVE };
   CHECK(vkCreateBuffer(dev, &bci, NULL, buf));
   VkMemoryRequirements mr;
   vkGetBufferMemoryRequirements(dev, *buf, &mr);
   VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .allocationSize = mr.size, .memoryTypeIndex = find_mem(mp, mr.memoryTypeBits, flags) };
   fprintf(stderr, "  alloc %llu bytes in type %u (flags 0x%x)\n",
           (unsigned long long)mr.size, mai.memoryTypeIndex, flags);
   CHECK(vkAllocateMemory(dev, &mai, NULL, mem));
   CHECK(vkBindBufferMemory(dev, *buf, *mem, 0));
}

int main(int argc, char **argv)
{
   const char *spv_path = argc > 1 ? argv[1] : "compute.spv";
   int do_copy = argc > 2 && !strcmp(argv[2], "copy");
   uint32_t n = argc > 3 ? (uint32_t)atoi(argv[3]) : 4096;
   const uint32_t mul = 3;
   VkDeviceSize size = (VkDeviceSize)n * 4;

   STEP("vkCreateInstance");
   VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .pApplicationName = "vk_compute_test", .apiVersion = VK_API_VERSION_1_3 };
   VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app };
   direct_driver_chain(&ici);
   VkInstance inst;
   CHECK(vkCreateInstance(&ici, NULL, &inst));

   uint32_t npd = 0;
   CHECK(vkEnumeratePhysicalDevices(inst, &npd, NULL));
   if (npd == 0) { fprintf(stderr, "no physical devices\n"); return 1; }
   VkPhysicalDevice pds[8];
   if (npd > 8) npd = 8;
   CHECK(vkEnumeratePhysicalDevices(inst, &npd, pds));
   VkPhysicalDevice pd = pds[0];
   VkPhysicalDeviceProperties props;
   vkGetPhysicalDeviceProperties(pd, &props);
   fprintf(stderr, "device: %s (vendor 0x%x device 0x%x, driver 0x%x)\n",
           props.deviceName, props.vendorID, props.deviceID, props.driverVersion);
   VkPhysicalDeviceMemoryProperties mp;
   vkGetPhysicalDeviceMemoryProperties(pd, &mp);
   for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
      fprintf(stderr, "  memtype %u: heap %u flags 0x%x\n", i, mp.memoryTypes[i].heapIndex,
              mp.memoryTypes[i].propertyFlags);

   uint32_t nqf = 0;
   vkGetPhysicalDeviceQueueFamilyProperties(pd, &nqf, NULL);
   VkQueueFamilyProperties qfp[16];
   if (nqf > 16) nqf = 16;
   vkGetPhysicalDeviceQueueFamilyProperties(pd, &nqf, qfp);
   uint32_t qf = UINT32_MAX;
   for (uint32_t i = 0; i < nqf; i++) {
      fprintf(stderr, "  queue family %u: flags 0x%x count %u\n", i, qfp[i].queueFlags, qfp[i].queueCount);
      if (qf == UINT32_MAX && (qfp[i].queueFlags & VK_QUEUE_COMPUTE_BIT))
         qf = i;
   }

   STEP("vkCreateDevice");
   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = qf, .queueCount = 1, .pQueuePriorities = &prio };
   VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
      .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci };
   VkDevice dev;
   CHECK(vkCreateDevice(pd, &dci, NULL, &dev));
   VkQueue q;
   vkGetDeviceQueue(dev, qf, 0, &q);

   STEP("buffers");
   VkBuffer hbuf, dbuf = VK_NULL_HANDLE;
   VkDeviceMemory hmem, dmem = VK_NULL_HANDLE;
   make_buffer(dev, &mp, size,
               VK_BUFFER_USAGE_STORAGE_BUFFER_BIT | VK_BUFFER_USAGE_TRANSFER_DST_BIT | VK_BUFFER_USAGE_TRANSFER_SRC_BIT,
               VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT, &hbuf, &hmem);
   uint32_t *ptr;
   CHECK(vkMapMemory(dev, hmem, 0, VK_WHOLE_SIZE, 0, (void **)&ptr));
   memset(ptr, 0xcd, size);
   if (do_copy)
      make_buffer(dev, &mp, size,
                  VK_BUFFER_USAGE_STORAGE_BUFFER_BIT | VK_BUFFER_USAGE_TRANSFER_SRC_BIT,
                  VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT, &dbuf, &dmem);
   VkBuffer target = do_copy ? dbuf : hbuf;

   STEP("pipeline");
   size_t spv_size;
   uint32_t *spv = read_file(spv_path, &spv_size);
   VkShaderModuleCreateInfo smci = { .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
      .codeSize = spv_size, .pCode = spv };
   VkShaderModule sm;
   CHECK(vkCreateShaderModule(dev, &smci, NULL, &sm));
   VkDescriptorSetLayoutBinding b = { .binding = 0, .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
      .descriptorCount = 1, .stageFlags = VK_SHADER_STAGE_COMPUTE_BIT };
   VkDescriptorSetLayoutCreateInfo dslci = { .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
      .bindingCount = 1, .pBindings = &b };
   VkDescriptorSetLayout dsl;
   CHECK(vkCreateDescriptorSetLayout(dev, &dslci, NULL, &dsl));
   VkPushConstantRange pcr = { .stageFlags = VK_SHADER_STAGE_COMPUTE_BIT, .offset = 0, .size = 8 };
   VkPipelineLayoutCreateInfo plci = { .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
      .setLayoutCount = 1, .pSetLayouts = &dsl, .pushConstantRangeCount = 1, .pPushConstantRanges = &pcr };
   VkPipelineLayout pl;
   CHECK(vkCreatePipelineLayout(dev, &plci, NULL, &pl));
   VkComputePipelineCreateInfo cpci = { .sType = VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO,
      .stage = { .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
                 .stage = VK_SHADER_STAGE_COMPUTE_BIT, .module = sm, .pName = "main" },
      .layout = pl };
   VkPipeline pipe;
   CHECK(vkCreateComputePipelines(dev, VK_NULL_HANDLE, 1, &cpci, NULL, &pipe));

   VkDescriptorPoolSize ps = { VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 1 };
   VkDescriptorPoolCreateInfo dpci = { .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
      .maxSets = 1, .poolSizeCount = 1, .pPoolSizes = &ps };
   VkDescriptorPool dp;
   CHECK(vkCreateDescriptorPool(dev, &dpci, NULL, &dp));
   VkDescriptorSetAllocateInfo dsai = { .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
      .descriptorPool = dp, .descriptorSetCount = 1, .pSetLayouts = &dsl };
   VkDescriptorSet ds;
   CHECK(vkAllocateDescriptorSets(dev, &dsai, &ds));
   VkDescriptorBufferInfo dbi = { target, 0, VK_WHOLE_SIZE };
   VkWriteDescriptorSet w = { .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = ds,
      .dstBinding = 0, .descriptorCount = 1, .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
      .pBufferInfo = &dbi };
   vkUpdateDescriptorSets(dev, 1, &w, 0, NULL);

   STEP("record");
   VkCommandPoolCreateInfo cpi = { .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
      .queueFamilyIndex = qf };
   VkCommandPool cp;
   CHECK(vkCreateCommandPool(dev, &cpi, NULL, &cp));
   VkCommandBufferAllocateInfo cbai = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
      .commandPool = cp, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1 };
   VkCommandBuffer cb;
   CHECK(vkAllocateCommandBuffers(dev, &cbai, &cb));
   VkCommandBufferBeginInfo cbbi = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
      .flags = 0 };
   CHECK(vkBeginCommandBuffer(cb, &cbbi));
   vkCmdBindPipeline(cb, VK_PIPELINE_BIND_POINT_COMPUTE, pipe);
   vkCmdBindDescriptorSets(cb, VK_PIPELINE_BIND_POINT_COMPUTE, pl, 0, 1, &ds, 0, NULL);
   uint32_t pc[2] = { n, mul };
   vkCmdPushConstants(cb, pl, VK_SHADER_STAGE_COMPUTE_BIT, 0, 8, pc);
   vkCmdDispatch(cb, (n + 63) / 64, 1, 1);
   VkMemoryBarrier mb = { .sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
      .srcAccessMask = VK_ACCESS_SHADER_WRITE_BIT,
      .dstAccessMask = do_copy ? VK_ACCESS_TRANSFER_READ_BIT : VK_ACCESS_HOST_READ_BIT };
   vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
                        do_copy ? VK_PIPELINE_STAGE_TRANSFER_BIT : VK_PIPELINE_STAGE_HOST_BIT,
                        0, 1, &mb, 0, NULL, 0, NULL);
   if (do_copy) {
      VkBufferCopy region = { 0, 0, size };
      vkCmdCopyBuffer(cb, dbuf, hbuf, 1, &region);
      VkMemoryBarrier mb2 = { .sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
         .srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT, .dstAccessMask = VK_ACCESS_HOST_READ_BIT };
      vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_HOST_BIT,
                           0, 1, &mb2, 0, NULL, 0, NULL);
   }
   CHECK(vkEndCommandBuffer(cb));

   STEP("submit + fence wait");
   VkFenceCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
   VkFence fence;
   CHECK(vkCreateFence(dev, &fci, NULL, &fence));
   VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cb };
   const char *loops_env = getenv("LOOPS");
   const int loops = loops_env ? atoi(loops_env) : 1;
   const int nowait = getenv("NOWAIT") != NULL;
   for (int it = 0; it < loops; it++) {
      if (!nowait || it == loops - 1) {
         CHECK(vkQueueSubmit(q, 1, &si, fence));
         VkResult wr = vkWaitForFences(dev, 1, &fence, VK_TRUE, 10ull * 1000 * 1000 * 1000);
         if (wr != VK_SUCCESS) { fprintf(stderr, "FAIL vkWaitForFences (iteration %d) -> %d\n", it, wr); return 1; }
         CHECK(vkResetFences(dev, 1, &fence));
      } else {
         VkResult sr = vkQueueSubmit(q, 1, &si, VK_NULL_HANDLE);
         if (sr != VK_SUCCESS) { fprintf(stderr, "FAIL vkQueueSubmit (iteration %d) -> %d\n", it, sr); return 1; }
      }
   }
   if (loops > 1) fprintf(stderr, "  %d submits done\n", loops);

   STEP("verify");
   uint32_t bad = 0;
   for (uint32_t i = 0; i < n; i++) {
      uint32_t want = i * mul + 7;
      if (ptr[i] != want) {
         if (bad < 8)
            fprintf(stderr, "  mismatch at %u: got 0x%08x want 0x%08x\n", i, ptr[i], want);
         bad++;
      }
   }
   printf("%s: %u/%u values correct (%s)\n", bad ? "FAIL" : "PASS", n - bad, n,
          do_copy ? "device-local + copy" : "host-visible");

   vkDestroyFence(dev, fence, NULL);
   vkDestroyCommandPool(dev, cp, NULL);
   vkDestroyDescriptorPool(dev, dp, NULL);
   vkDestroyPipeline(dev, pipe, NULL);
   vkDestroyPipelineLayout(dev, pl, NULL);
   vkDestroyDescriptorSetLayout(dev, dsl, NULL);
   vkDestroyShaderModule(dev, sm, NULL);
   vkUnmapMemory(dev, hmem);
   vkDestroyBuffer(dev, hbuf, NULL);
   vkFreeMemory(dev, hmem, NULL);
   if (do_copy) { vkDestroyBuffer(dev, dbuf, NULL); vkFreeMemory(dev, dmem, NULL); }
   vkDestroyDevice(dev, NULL);
   vkDestroyInstance(inst, NULL);
   return bad ? 1 : 0;
}
