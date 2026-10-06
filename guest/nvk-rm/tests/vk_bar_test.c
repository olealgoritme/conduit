/* Host-visible VRAM test: a buffer in a DEVICE_LOCAL | HOST_VISIBLE memory
 * type (the BAR heap), written by the CPU, transformed in place by a compute
 * dispatch (data[i] = data[i] * 3 + 7), read back by the CPU and verified.
 * Also times CPU writes and reads through the mapping and repeated map/unmap
 * of the same memory, and (with "fill") allocates 16 MiB blocks until the
 * heap refuses, to check that the heap size is enforced.
 *
 *   vk_bar_test bar.comp.spv [N] [fill]
 */
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <time.h>

#include "vk_direct_driver.h"

#define CHECK(x) do { VkResult r_ = (x); \
   if (r_ != VK_SUCCESS) { fprintf(stderr, "FAIL %s:%d: %s -> %d\n", __FILE__, __LINE__, #x, r_); exit(1); } \
   } while (0)

static double now_s(void)
{
#ifdef _WIN32
   LARGE_INTEGER f, c;
   QueryPerformanceFrequency(&f);
   QueryPerformanceCounter(&c);
   return (double)c.QuadPart / (double)f.QuadPart;
#else
   struct timespec ts;
   clock_gettime(CLOCK_MONOTONIC, &ts);
   return ts.tv_sec + ts.tv_nsec * 1e-9;
#endif
}

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

int main(int argc, char **argv)
{
   const char *spv_path = argc > 1 ? argv[1] : "bar.comp.spv";
   uint32_t n = argc > 2 ? (uint32_t)atoi(argv[2]) : (4u << 20); /* 16 MiB */
   const int do_fill = argc > 3 && !strcmp(argv[3], "fill");
   const uint32_t mul = 3;
   const VkDeviceSize size = (VkDeviceSize)n * 4;

   VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .pApplicationName = "vk_bar_test", .apiVersion = VK_API_VERSION_1_3 };
   VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app };
   direct_driver_chain(&ici);
   VkInstance inst;
   CHECK(vkCreateInstance(&ici, NULL, &inst));
   uint32_t npd = 1;
   VkPhysicalDevice pd;
   VkResult er = vkEnumeratePhysicalDevices(inst, &npd, &pd);
   if ((er != VK_SUCCESS && er != VK_INCOMPLETE) || npd == 0) {
      fprintf(stderr, "no physical devices\n");
      return 1;
   }
   VkPhysicalDeviceProperties props;
   vkGetPhysicalDeviceProperties(pd, &props);
   printf("device: %s\n", props.deviceName);

   VkPhysicalDeviceMemoryProperties mp;
   vkGetPhysicalDeviceMemoryProperties(pd, &mp);
   for (uint32_t i = 0; i < mp.memoryHeapCount; i++)
      printf("  heap %u: %llu MiB flags 0x%x\n", i,
             (unsigned long long)(mp.memoryHeaps[i].size >> 20), mp.memoryHeaps[i].flags);
   const VkMemoryPropertyFlags want = VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT |
                                      VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                                      VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;
   uint32_t bar_type = UINT32_MAX;
   for (uint32_t i = 0; i < mp.memoryTypeCount; i++) {
      printf("  memtype %u: heap %u flags 0x%x\n", i, mp.memoryTypes[i].heapIndex,
             mp.memoryTypes[i].propertyFlags);
      if (bar_type == UINT32_MAX && (mp.memoryTypes[i].propertyFlags & want) == want)
         bar_type = i;
   }
   if (bar_type == UINT32_MAX) {
      printf("FAIL: no DEVICE_LOCAL | HOST_VISIBLE | HOST_COHERENT memory type\n");
      return 1;
   }
   const uint32_t bar_heap = mp.memoryTypes[bar_type].heapIndex;
   const uint64_t heap_MiB = mp.memoryHeaps[bar_heap].size >> 20;
   printf("host-visible VRAM: type %u, heap %u (%llu MiB)\n", bar_type, bar_heap,
          (unsigned long long)heap_MiB);

   uint32_t nqf = 16;
   VkQueueFamilyProperties qfp[16];
   vkGetPhysicalDeviceQueueFamilyProperties(pd, &nqf, qfp);
   uint32_t qf = 0;
   while (qf < nqf && !(qfp[qf].queueFlags & VK_QUEUE_COMPUTE_BIT))
      qf++;
   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = qf, .queueCount = 1, .pQueuePriorities = &prio };
   VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
      .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci };
   VkDevice dev;
   CHECK(vkCreateDevice(pd, &dci, NULL, &dev));
   VkQueue q;
   vkGetDeviceQueue(dev, qf, 0, &q);

   VkBufferCreateInfo bci = { .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
      .size = size, .usage = VK_BUFFER_USAGE_STORAGE_BUFFER_BIT,
      .sharingMode = VK_SHARING_MODE_EXCLUSIVE };
   VkBuffer buf;
   CHECK(vkCreateBuffer(dev, &bci, NULL, &buf));
   VkMemoryRequirements mr;
   vkGetBufferMemoryRequirements(dev, buf, &mr);
   if (!(mr.memoryTypeBits & (1u << bar_type))) {
      printf("FAIL: storage buffer cannot live in type %u (bits 0x%x)\n", bar_type,
             mr.memoryTypeBits);
      return 1;
   }
   VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .allocationSize = mr.size, .memoryTypeIndex = bar_type };
   VkDeviceMemory mem;
   double t0 = now_s();
   CHECK(vkAllocateMemory(dev, &mai, NULL, &mem));
   double t1 = now_s();
   CHECK(vkBindBufferMemory(dev, buf, mem, 0));
   printf("vkAllocateMemory %llu MiB (incl. its BAR mapping): %.2f ms\n",
          (unsigned long long)(mr.size >> 20), (t1 - t0) * 1e3);

   /* Repeated map/unmap must not go to RM each time */
   const int maps = 1000;
   uint32_t *ptr = NULL;
   t0 = now_s();
   for (int i = 0; i < maps; i++) {
      CHECK(vkMapMemory(dev, mem, 0, VK_WHOLE_SIZE, 0, (void **)&ptr));
      vkUnmapMemory(dev, mem);
   }
   t1 = now_s();
   printf("map+unmap: %.3f us each (%d)\n", (t1 - t0) * 1e6 / maps, maps);

   CHECK(vkMapMemory(dev, mem, 0, VK_WHOLE_SIZE, 0, (void **)&ptr));
   uint32_t *src = malloc(size);
   for (uint32_t i = 0; i < n; i++)
      src[i] = i ^ 0x5a5a0000u;
   t0 = now_s();
   memcpy(ptr, src, size);
   t1 = now_s();
   printf("CPU write %llu MiB: %.2f ms (%.0f MB/s)\n", (unsigned long long)(size >> 20),
          (t1 - t0) * 1e3, size / (t1 - t0) / 1e6);

   size_t spv_size;
   uint32_t *spv = read_file(spv_path, &spv_size);
   VkShaderModuleCreateInfo smci = { .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
      .codeSize = spv_size, .pCode = spv };
   VkShaderModule sm;
   CHECK(vkCreateShaderModule(dev, &smci, NULL, &sm));
   VkDescriptorSetLayoutBinding b = { .binding = 0,
      .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
      .descriptorCount = 1, .stageFlags = VK_SHADER_STAGE_COMPUTE_BIT };
   VkDescriptorSetLayoutCreateInfo dslci = {
      .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
      .bindingCount = 1, .pBindings = &b };
   VkDescriptorSetLayout dsl;
   CHECK(vkCreateDescriptorSetLayout(dev, &dslci, NULL, &dsl));
   VkPushConstantRange pcr = { .stageFlags = VK_SHADER_STAGE_COMPUTE_BIT, .offset = 0, .size = 8 };
   VkPipelineLayoutCreateInfo plci = { .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
      .setLayoutCount = 1, .pSetLayouts = &dsl, .pushConstantRangeCount = 1,
      .pPushConstantRanges = &pcr };
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
   VkDescriptorBufferInfo dbi = { buf, 0, VK_WHOLE_SIZE };
   VkWriteDescriptorSet w = { .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = ds,
      .dstBinding = 0, .descriptorCount = 1,
      .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, .pBufferInfo = &dbi };
   vkUpdateDescriptorSets(dev, 1, &w, 0, NULL);

   VkCommandPoolCreateInfo cpi = { .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
      .queueFamilyIndex = qf };
   VkCommandPool cp;
   CHECK(vkCreateCommandPool(dev, &cpi, NULL, &cp));
   VkCommandBufferAllocateInfo cbai = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
      .commandPool = cp, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1 };
   VkCommandBuffer cb;
   CHECK(vkAllocateCommandBuffers(dev, &cbai, &cb));
   VkCommandBufferBeginInfo cbbi = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO };
   CHECK(vkBeginCommandBuffer(cb, &cbbi));
   VkMemoryBarrier hb = { .sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
      .srcAccessMask = VK_ACCESS_HOST_WRITE_BIT,
      .dstAccessMask = VK_ACCESS_SHADER_READ_BIT | VK_ACCESS_SHADER_WRITE_BIT };
   vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_HOST_BIT, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
                        0, 1, &hb, 0, NULL, 0, NULL);
   vkCmdBindPipeline(cb, VK_PIPELINE_BIND_POINT_COMPUTE, pipe);
   vkCmdBindDescriptorSets(cb, VK_PIPELINE_BIND_POINT_COMPUTE, pl, 0, 1, &ds, 0, NULL);
   uint32_t pc[2] = { n, mul };
   vkCmdPushConstants(cb, pl, VK_SHADER_STAGE_COMPUTE_BIT, 0, 8, pc);
   vkCmdDispatch(cb, (n + 63) / 64, 1, 1);
   VkMemoryBarrier mb = { .sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
      .srcAccessMask = VK_ACCESS_SHADER_WRITE_BIT, .dstAccessMask = VK_ACCESS_HOST_READ_BIT };
   vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_PIPELINE_STAGE_HOST_BIT,
                        0, 1, &mb, 0, NULL, 0, NULL);
   CHECK(vkEndCommandBuffer(cb));
   VkFenceCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
   VkFence fence;
   CHECK(vkCreateFence(dev, &fci, NULL, &fence));
   VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1,
      .pCommandBuffers = &cb };
   CHECK(vkQueueSubmit(q, 1, &si, fence));
   CHECK(vkWaitForFences(dev, 1, &fence, VK_TRUE, 10ull * 1000 * 1000 * 1000));

   uint32_t *back = malloc(size);
   t0 = now_s();
   memcpy(back, ptr, size);
   t1 = now_s();
   printf("CPU read %llu MiB: %.2f ms (%.0f MB/s; reads through a WC mapping are slow)\n",
          (unsigned long long)(size >> 20), (t1 - t0) * 1e3, size / (t1 - t0) / 1e6);
   uint32_t bad = 0;
   for (uint32_t i = 0; i < n; i++) {
      const uint32_t want = src[i] * mul + 7u;
      if (back[i] != want) {
         if (bad < 8)
            printf("  mismatch at %u: got 0x%08x want 0x%08x\n", i, back[i], want);
         bad++;
      }
   }
   printf("%s: %u/%u values correct (CPU write -> GPU read/write -> CPU read, "
          "host-visible VRAM)\n", bad ? "FAIL" : "PASS", n - bad, n);

   int fill_bad = 0;
   if (do_fill) {
      /* 16 MiB blocks until the heap is full; the heap size must hold */
      VkDeviceMemory blocks[256];
      uint32_t nb = 0;
      VkMemoryAllocateInfo fai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
         .allocationSize = 16u << 20, .memoryTypeIndex = bar_type };
      VkResult fr = VK_SUCCESS;
      t0 = now_s();
      while (nb < 256 &&
             (fr = vkAllocateMemory(dev, &fai, NULL, &blocks[nb])) == VK_SUCCESS) {
         void *p;
         CHECK(vkMapMemory(dev, blocks[nb], 0, VK_WHOLE_SIZE, 0, &p));
         ((volatile uint32_t *)p)[0] = nb;
         nb++;
      }
      t1 = now_s();
      const uint64_t got = (uint64_t)nb * 16 + (mr.size >> 20);
      printf("fill: %u x 16 MiB more, then %d (%llu MiB in use, heap %llu MiB), "
             "%.2f ms per block\n", nb, fr, (unsigned long long)got,
             (unsigned long long)heap_MiB, nb ? (t1 - t0) * 1e3 / nb : 0.0);
      if (fr != VK_ERROR_OUT_OF_DEVICE_MEMORY || got > heap_MiB)
         fill_bad = 1;
      for (uint32_t i = 0; i < nb; i++)
         vkFreeMemory(dev, blocks[i], NULL);
      /* Freed space must be usable again */
      VkDeviceMemory again;
      CHECK(vkAllocateMemory(dev, &fai, NULL, &again));
      vkFreeMemory(dev, again, NULL);
      printf("%s: heap limit enforced, freed space reusable\n", fill_bad ? "FAIL" : "PASS");
   }

   vkDestroyFence(dev, fence, NULL);
   vkDestroyCommandPool(dev, cp, NULL);
   vkDestroyDescriptorPool(dev, dp, NULL);
   vkDestroyPipeline(dev, pipe, NULL);
   vkDestroyPipelineLayout(dev, pl, NULL);
   vkDestroyDescriptorSetLayout(dev, dsl, NULL);
   vkDestroyShaderModule(dev, sm, NULL);
   vkUnmapMemory(dev, mem);
   vkDestroyBuffer(dev, buf, NULL);
   vkFreeMemory(dev, mem, NULL);
   vkDestroyDevice(dev, NULL);
   vkDestroyInstance(inst, NULL);
   return (bad || fill_bad) ? 1 : 0;
}
