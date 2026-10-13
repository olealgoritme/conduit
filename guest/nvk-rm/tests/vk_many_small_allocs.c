/* vk_many_small_allocs: many small system memory objects alive at once, the
 * way a native Vulkan engine creates them: descriptor pools, command pools
 * with a recorded command buffer each, query pools and small host-visible
 * memory allocations, thousands of each, all checked to succeed; the GPU
 * fills a buffer in every memory allocation at an offset and the CPU reads it
 * back, before and after freeing and allocating half of them again; then
 * everything is freed.
 *
 * On NVK on RM every such object used to be its own registration of our
 * pages with RM (a KMD pin, at most a few hundred per process); past that the
 * fallback ran out of KMD handles and creation failed with
 * VK_ERROR_MEMORY_MAP_FAILED, and a failed vkCreateDescriptorPool crashed in
 * its error path.
 *
 *   vk_many_small_allocs [count] [device name substring, default "NVK"]
 *
 * Prints a RESULT line; exit code 0 on PASS.  Windows: HELIOS_ICD=nvk, or
 * VK_DIRECT_DRIVER (tests/vk_direct_driver.h).
 */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <vulkan/vulkan.h>
#include "vk_direct_driver.h"

#define CHECK(x, what)                                                        \
   do {                                                                       \
      VkResult r_ = (x);                                                      \
      if (r_ != VK_SUCCESS) {                                                 \
         printf("%s failed at %u: %d\n", #what, i, r_);                        \
         failed = 1;                                                          \
         goto done_##what;                                                    \
      }                                                                       \
   } while (0)

int main(int argc, char **argv)
{
   const uint32_t count = argc > 1 ? (uint32_t)atoi(argv[1]) : 3000;
   const char *want = argc > 2 ? argv[2] : "NVK";
   int failed = 0;
   uint32_t i = 0;

   VkApplicationInfo ai = {VK_STRUCTURE_TYPE_APPLICATION_INFO, NULL, "vk_many_small_allocs", 1, NULL, 0, VK_API_VERSION_1_1};
   VkInstanceCreateInfo ici = {VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, NULL, 0, &ai};
   direct_driver_chain(&ici);
   VkInstance inst;
   if (vkCreateInstance(&ici, NULL, &inst) != VK_SUCCESS) {
      printf("RESULT FAIL (no instance)\n");
      return 1;
   }
   uint32_t n = 0;
   vkEnumeratePhysicalDevices(inst, &n, NULL);
   VkPhysicalDevice pds[16];
   if (n > 16) n = 16;
   vkEnumeratePhysicalDevices(inst, &n, pds);
   VkPhysicalDevice pd = VK_NULL_HANDLE;
   VkPhysicalDeviceProperties props;
   for (uint32_t d = 0; d < n; d++) {
      vkGetPhysicalDeviceProperties(pds[d], &props);
      if (strstr(props.deviceName, want)) { pd = pds[d]; break; }
   }
   if (!pd) {
      printf("RESULT FAIL (no device matching \"%s\")\n", want);
      return 1;
   }
   printf("device %s, %u of each\n", props.deviceName, count);

   VkPhysicalDeviceMemoryProperties mp;
   vkGetPhysicalDeviceMemoryProperties(pd, &mp);
   /* Plain system memory: host-visible without DEVICE_LOCAL (a BAR heap's
    * type would not be system memory at all) */
   uint32_t host_type = UINT32_MAX;
   for (uint32_t t = 0; t < mp.memoryTypeCount; t++) {
      const VkMemoryPropertyFlags f = mp.memoryTypes[t].propertyFlags;
      if ((f & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT) && (f & VK_MEMORY_PROPERTY_HOST_COHERENT_BIT) &&
          !(f & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT)) {
         host_type = t;
         break;
      }
   }

   if (host_type == UINT32_MAX) {
      printf("RESULT FAIL (no host-visible system memory type)\n");
      return 1;
   }

   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = {VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, NULL, 0, 0, 1, &prio};
   VkDeviceCreateInfo dci = {VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, NULL, 0, 1, &qci};
   VkDevice dev;
   if (vkCreateDevice(pd, &dci, NULL, &dev) != VK_SUCCESS) {
      printf("RESULT FAIL (no device)\n");
      return 1;
   }

   VkDescriptorPool *dpools = calloc(count, sizeof(*dpools));
   VkCommandPool *cpools = calloc(count, sizeof(*cpools));
   VkQueryPool *qpools = calloc(count, sizeof(*qpools));
   VkDeviceMemory *mems = calloc(count, sizeof(*mems));
   uint32_t ndp = 0, ncp = 0, nqp = 0, nmem = 0;

   /* Descriptor pools of a few dozen descriptors, as engines make per frame
    * and thread */
   for (i = 0; i < count; i++) {
      VkDescriptorPoolSize sizes[3] = {
         {VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, 32 + (i % 64)},
         {VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER_DYNAMIC, 8},
         {VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 8},
      };
      VkDescriptorPoolCreateInfo dpci = {VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO, NULL, 0, 16, 3, sizes};
      CHECK(vkCreateDescriptorPool(dev, &dpci, NULL, &dpools[ndp]), dpool);
      ndp++;
   }
done_dpool:
   /* Command pools, each with one recorded command buffer (a pushbuffer
    * chunk each) */
   for (i = 0; !failed && i < count; i++) {
      VkCommandPoolCreateInfo cpci = {VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, NULL, 0, 0};
      CHECK(vkCreateCommandPool(dev, &cpci, NULL, &cpools[ncp]), cpool);
      ncp++;
      VkCommandBufferAllocateInfo cbai = {VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, NULL, cpools[ncp - 1],
                                          VK_COMMAND_BUFFER_LEVEL_PRIMARY, 1};
      VkCommandBuffer cb;
      CHECK(vkAllocateCommandBuffers(dev, &cbai, &cb), cpool);
      VkCommandBufferBeginInfo bi = {VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};
      CHECK(vkBeginCommandBuffer(cb, &bi), cpool);
      vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, 0, 0, NULL, 0, NULL, 0, NULL);
      CHECK(vkEndCommandBuffer(cb), cpool);
   }
done_cpool:
   for (i = 0; !failed && i < count; i++) {
      VkQueryPoolCreateInfo qpci = {VK_STRUCTURE_TYPE_QUERY_POOL_CREATE_INFO, NULL, 0, VK_QUERY_TYPE_TIMESTAMP, 64};
      CHECK(vkCreateQueryPool(dev, &qpci, NULL, &qpools[nqp]), qpool);
      nqp++;
   }
done_qpool:
   /* Small system memory allocations, each with a buffer the GPU fills
    * (vkCmdFillBuffer) at an offset with a value of its own, read back on
    * the CPU: the GPU's view of every range has to be the CPU's.  Then every
    * other one is freed and allocated again (reused ranges) and the check
    * repeats.  Bytes outside the filled part must read zero.
    */
   VkBuffer *bufs = calloc(count, sizeof(*bufs));
   VkQueue queue;
   vkGetDeviceQueue(dev, 0, 0, &queue);
   VkCommandPool fill_pool;
   VkCommandPoolCreateInfo fpci = {VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, NULL,
                                   VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT, 0};
   if (vkCreateCommandPool(dev, &fpci, NULL, &fill_pool) != VK_SUCCESS) {
      printf("RESULT FAIL (no command pool)\n");
      return 1;
   }
   VkFence fence;
   VkFenceCreateInfo fci = {VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
   if (vkCreateFence(dev, &fci, NULL, &fence) != VK_SUCCESS) {
      printf("RESULT FAIL (no fence)\n");
      return 1;
   }
   for (uint32_t round = 0; !failed && host_type != UINT32_MAX && round < 2; round++) {
      for (i = 0; i < count; i++) {
         if (round == 1 && (i & 1))
            continue; /* round 1 keeps the odd ones */
         if (round == 1) {
            vkDestroyBuffer(dev, bufs[i], NULL);
            vkFreeMemory(dev, mems[i], NULL);
            bufs[i] = VK_NULL_HANDLE;
            mems[i] = VK_NULL_HANDLE;
            nmem--;
         }
      }
      for (i = 0; i < count; i++) {
         if (round == 1 && (i & 1))
            continue;
         const VkDeviceSize size = 4096u << ((i + round) % 5);
         VkBufferCreateInfo bci = {VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, NULL, 0, size,
                                   VK_BUFFER_USAGE_TRANSFER_DST_BIT, VK_SHARING_MODE_EXCLUSIVE};
         CHECK(vkCreateBuffer(dev, &bci, NULL, &bufs[i]), mem);
         VkMemoryRequirements req;
         vkGetBufferMemoryRequirements(dev, bufs[i], &req);
         if (!(req.memoryTypeBits & (1u << host_type)) || req.size > size) {
            printf("buffer %u cannot live in type %u (bits 0x%x, size %llu)\n", i, host_type,
                   req.memoryTypeBits, (unsigned long long)req.size);
            failed = 1;
            goto done_mem;
         }
         VkMemoryAllocateInfo mai = {VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, NULL, size, host_type};
         CHECK(vkAllocateMemory(dev, &mai, NULL, &mems[i]), mem);
         nmem++;
         CHECK(vkBindBufferMemory(dev, bufs[i], mems[i], 0), mem);
      }
      VkCommandBuffer cb;
      VkCommandBufferAllocateInfo cbai = {VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, NULL, fill_pool,
                                          VK_COMMAND_BUFFER_LEVEL_PRIMARY, 1};
      CHECK(vkAllocateCommandBuffers(dev, &cbai, &cb), mem);
      VkCommandBufferBeginInfo bi = {VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO, NULL,
                                     VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT};
      CHECK(vkBeginCommandBuffer(cb, &bi), mem);
      for (i = 0; i < count; i++) {
         const VkDeviceSize size = 4096u << ((i + (round == 1 && !(i & 1))) % 5);
         vkCmdFillBuffer(cb, bufs[i], 256, size - 512, 0x5a000000u | (round << 16) | i);
      }
      VkMemoryBarrier mb = {VK_STRUCTURE_TYPE_MEMORY_BARRIER, NULL, VK_ACCESS_TRANSFER_WRITE_BIT, VK_ACCESS_HOST_READ_BIT};
      vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_HOST_BIT, 0, 1, &mb, 0, NULL, 0, NULL);
      CHECK(vkEndCommandBuffer(cb), mem);
      VkSubmitInfo si = {VK_STRUCTURE_TYPE_SUBMIT_INFO, NULL, 0, NULL, NULL, 1, &cb};
      CHECK(vkResetFences(dev, 1, &fence), mem);
      CHECK(vkQueueSubmit(queue, 1, &si, fence), mem);
      CHECK(vkWaitForFences(dev, 1, &fence, VK_TRUE, 10ull * 1000 * 1000 * 1000), mem);
      vkFreeCommandBuffers(dev, fill_pool, 1, &cb);
      for (i = 0; !failed && i < count; i++) {
         const int refilled = round == 1 && !(i & 1);
         const VkDeviceSize size = 4096u << ((i + refilled) % 5);
         const uint32_t value = 0x5a000000u | (round << 16) | i;
         uint32_t *p;
         CHECK(vkMapMemory(dev, mems[i], 0, VK_WHOLE_SIZE, 0, (void **)&p), mem);
         for (VkDeviceSize w = 0; w < size / 4; w++) {
            const uint32_t expect = (w >= 64 && w < size / 4 - 64) ? value : 0;
            if (p[w] != expect) {
               printf("round %u allocation %u word %llu reads 0x%08x, expected 0x%08x\n", round, i,
                      (unsigned long long)w, p[w], expect);
               failed = 1;
               break;
            }
         }
         vkUnmapMemory(dev, mems[i]);
      }
   }
done_mem:
   vkDestroyFence(dev, fence, NULL);
   vkDestroyCommandPool(dev, fill_pool, NULL);

   printf("created %u descriptor pools, %u command pools, %u query pools, %u memories\n", ndp, ncp, nqp, nmem);
   for (i = 0; i < count; i++) {
      if (bufs[i]) vkDestroyBuffer(dev, bufs[i], NULL);
      if (mems[i]) vkFreeMemory(dev, mems[i], NULL);
   }
   for (i = 0; i < nqp; i++) vkDestroyQueryPool(dev, qpools[i], NULL);
   for (i = 0; i < ncp; i++) vkDestroyCommandPool(dev, cpools[i], NULL);
   for (i = 0; i < ndp; i++) vkDestroyDescriptorPool(dev, dpools[i], NULL);
   vkDestroyDevice(dev, NULL);
   vkDestroyInstance(inst, NULL);
   printf("RESULT %s\n", failed ? "FAIL" : "PASS");
   return failed;
}
