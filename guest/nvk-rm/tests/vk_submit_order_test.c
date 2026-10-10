/* vk_submit_order_test: does a signalled fence / timeline value mean the work is done?
 *
 * Submits N small command buffers (one 32-thread dispatch each that spins a
 * few microseconds, then writes its sequence number) round-robin over K
 * recycled command buffers, the way DXVK recycles its command lists: before
 * re-recording slot i%K it waits on that slot's fence and then checks that
 * the result the GPU wrote for submission i-K is there.  A second thread
 * waits on the timeline semaphore for random recent values and checks the
 * results the same way (several CPU waiters on one channel).  Enough
 * submissions to wrap the GPFIFO ring several times.  GPU load: idle
 * apart from microsecond dispatches.
 *
 *   vk_submit_order_test [device-substring] [submissions] [spin]
 */
#ifdef _WIN32
#include <windows.h>
#else
#include <pthread.h>
#endif
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>

#include "submit_seq_comp.h"

#define CHECK(x) do { VkResult r_ = (x); if (r_ != VK_SUCCESS) { \
   fprintf(stderr, "FAIL %s:%d: %s -> %d\n", __FILE__, __LINE__, #x, r_); exit(1); } } while (0)

#define K 8

static VkDevice dev;
static VkSemaphore timeline;
static volatile uint32_t *results;
static volatile uint32_t submitted;  /* highest timeline value submitted */
static volatile int done;
static uint32_t total;
static volatile uint32_t waiter_checks, waiter_bad;

static uint64_t timeline_value(void)
{
   uint64_t v = 0;
   vkGetSemaphoreCounterValue(dev, timeline, &v);
   return v;
}

#ifdef _WIN32
static DWORD WINAPI waiter(LPVOID arg)
#else
static void *waiter(void *arg)
#endif
{
   (void)arg;
   uint32_t rng = 12345;
   while (!done) {
      uint32_t top = submitted;
      if (top < 2)
         continue;
      rng = rng * 1664525u + 1013904223u;
      uint32_t want = top - (rng >> 16) % (top < 16 ? top - 1 : 16);
      uint64_t v = want;
      VkSemaphoreWaitInfo wi = { VK_STRUCTURE_TYPE_SEMAPHORE_WAIT_INFO, .semaphoreCount = 1,
         .pSemaphores = &timeline, .pValues = &v };
      VkResult r = vkWaitSemaphores(dev, &wi, 2000000000ull);
      if (r != VK_SUCCESS) {
         fprintf(stderr, "waiter: vkWaitSemaphores(%u) -> %d\n", want, r);
         waiter_bad++;
         continue;
      }
      /* submission s signals timeline value s + 1 */
      uint32_t s = want - 1;
      waiter_checks++;
      if (s + 65536 > submitted && results[s % 65536] != s) {
         if (waiter_bad++ < 10)
            fprintf(stderr, "waiter: timeline reached %u but result[%u] = %u (counter now %llu)\n",
                    want, s, results[s % 65536], (unsigned long long)timeline_value());
      }
   }
   return 0;
}

int main(int argc, char **argv)
{
   const char *want = argc > 1 ? argv[1] : "NVK";
   total = argc > 2 ? (uint32_t)atoi(argv[2]) : 6000;
   uint32_t spin = argc > 3 ? (uint32_t)atoi(argv[3]) : 2000;

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
   printf("using: %s, %u submissions, spin %u\n", props.deviceName, total, spin);
   VkPhysicalDeviceMemoryProperties memprops;
   vkGetPhysicalDeviceMemoryProperties(pd, &memprops);

   uint32_t qf = 0, nqf = 16;
   VkQueueFamilyProperties qfp[16];
   vkGetPhysicalDeviceQueueFamilyProperties(pd, &nqf, qfp);
   for (qf = 0; qf < nqf; qf++)
      if (qfp[qf].queueFlags & VK_QUEUE_GRAPHICS_BIT)
         break;
   VkPhysicalDeviceVulkan12Features f12 = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES,
      .timelineSemaphore = VK_TRUE };
   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = { VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = qf, .queueCount = 1, .pQueuePriorities = &prio };
   VkDeviceCreateInfo dci = { VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, &f12,
      .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci };
   CHECK(vkCreateDevice(pd, &dci, NULL, &dev));
   VkQueue q;
   vkGetDeviceQueue(dev, qf, 0, &q);

   VkBufferCreateInfo bci = { VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size = 65536 * 4,
      .usage = VK_BUFFER_USAGE_STORAGE_BUFFER_BIT };
   VkBuffer buf;
   CHECK(vkCreateBuffer(dev, &bci, NULL, &buf));
   VkMemoryRequirements mr;
   vkGetBufferMemoryRequirements(dev, buf, &mr);
   uint32_t mt = 0;
   for (mt = 0; mt < memprops.memoryTypeCount; mt++) {
      VkMemoryPropertyFlags f = memprops.memoryTypes[mt].propertyFlags;
      if ((mr.memoryTypeBits & (1u << mt)) &&
          (f & (VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT)) ==
             (VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT))
         break;
   }
   VkMemoryAllocateInfo mai = { VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = mr.size,
      .memoryTypeIndex = mt };
   VkDeviceMemory mem;
   CHECK(vkAllocateMemory(dev, &mai, NULL, &mem));
   CHECK(vkBindBufferMemory(dev, buf, mem, 0));
   void *map;
   CHECK(vkMapMemory(dev, mem, 0, VK_WHOLE_SIZE, 0, &map));
   results = map;
   memset(map, 0xff, 65536 * 4);

   VkDescriptorSetLayoutBinding b0 = { 0, VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 1, VK_SHADER_STAGE_COMPUTE_BIT };
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
   VkDescriptorBufferInfo dbi = { buf, 0, VK_WHOLE_SIZE };
   VkWriteDescriptorSet w = { VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = ds, .dstBinding = 0,
      .descriptorCount = 1, .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, .pBufferInfo = &dbi };
   vkUpdateDescriptorSets(dev, 1, &w, 0, NULL);
   VkPushConstantRange pcr = { VK_SHADER_STAGE_COMPUTE_BIT, 0, 8 };
   VkPipelineLayoutCreateInfo plci = { VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
      .setLayoutCount = 1, .pSetLayouts = &dsl, .pushConstantRangeCount = 1, .pPushConstantRanges = &pcr };
   VkPipelineLayout pl;
   CHECK(vkCreatePipelineLayout(dev, &plci, NULL, &pl));
   VkShaderModuleCreateInfo smci = { VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
      .codeSize = sizeof(submit_seq_comp), .pCode = submit_seq_comp };
   VkShaderModule cs;
   CHECK(vkCreateShaderModule(dev, &smci, NULL, &cs));
   VkComputePipelineCreateInfo cpci = { VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO,
      .stage = { VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, .stage = VK_SHADER_STAGE_COMPUTE_BIT,
                 .module = cs, .pName = "main" }, .layout = pl };
   VkPipeline pipe;
   CHECK(vkCreateComputePipelines(dev, VK_NULL_HANDLE, 1, &cpci, NULL, &pipe));

   VkSemaphoreTypeCreateInfo stci = { VK_STRUCTURE_TYPE_SEMAPHORE_TYPE_CREATE_INFO,
      .semaphoreType = VK_SEMAPHORE_TYPE_TIMELINE, .initialValue = 0 };
   VkSemaphoreCreateInfo sci = { VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO, &stci };
   CHECK(vkCreateSemaphore(dev, &sci, NULL, &timeline));

   VkCommandPool pools[K];
   VkCommandBuffer cbs[K];
   VkFence fences[K];
   uint32_t slot_seq[K];
   for (int i = 0; i < K; i++) {
      VkCommandPoolCreateInfo cpi = { VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .queueFamilyIndex = qf };
      CHECK(vkCreateCommandPool(dev, &cpi, NULL, &pools[i]));
      VkCommandBufferAllocateInfo cbai = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
         .commandPool = pools[i], .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1 };
      CHECK(vkAllocateCommandBuffers(dev, &cbai, &cbs[i]));
      VkFenceCreateInfo fci = { VK_STRUCTURE_TYPE_FENCE_CREATE_INFO, .flags = VK_FENCE_CREATE_SIGNALED_BIT };
      CHECK(vkCreateFence(dev, &fci, NULL, &fences[i]));
      slot_seq[i] = ~0u;
   }

#ifdef _WIN32
   HANDLE th = CreateThread(NULL, 0, waiter, NULL, 0, NULL);
#else
   pthread_t th;
   pthread_create(&th, NULL, waiter, NULL);
#endif

   uint32_t fence_bad = 0, poll_bad = 0;
   for (uint32_t s = 0; s < total; s++) {
      int k = s % K;
      CHECK(vkWaitForFences(dev, 1, &fences[k], VK_TRUE, 2000000000ull));
      if (slot_seq[k] != ~0u) {
         uint32_t prev = slot_seq[k];
         if (results[prev % 65536] != prev) {
            if (fence_bad++ < 10)
               fprintf(stderr, "fence of submission %u signalled but result = %u (timeline %llu)\n",
                       prev, results[prev % 65536], (unsigned long long)timeline_value());
         }
      }
      /* Polling path, as DXVK's vkGetFenceStatus-style checks */
      if (s >= 2 * K) {
         uint64_t tv = timeline_value();
         if (tv > 0 && tv <= s) {
            uint32_t chk = (uint32_t)tv - 1;
            if (results[chk % 65536] != chk && poll_bad++ < 10)
               fprintf(stderr, "timeline counter %llu but result[%u] = %u\n",
                       (unsigned long long)tv, chk, results[chk % 65536]);
         }
      }
      CHECK(vkResetFences(dev, 1, &fences[k]));
      CHECK(vkResetCommandPool(dev, pools[k], 0));
      VkCommandBufferBeginInfo cbbi = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
         .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT };
      CHECK(vkBeginCommandBuffer(cbs[k], &cbbi));
      vkCmdBindPipeline(cbs[k], VK_PIPELINE_BIND_POINT_COMPUTE, pipe);
      vkCmdBindDescriptorSets(cbs[k], VK_PIPELINE_BIND_POINT_COMPUTE, pl, 0, 1, &ds, 0, NULL);
      uint32_t pc[2] = { s, spin };
      vkCmdPushConstants(cbs[k], pl, VK_SHADER_STAGE_COMPUTE_BIT, 0, 8, pc);
      vkCmdDispatch(cbs[k], 1, 1, 1);
      VkMemoryBarrier mb = { VK_STRUCTURE_TYPE_MEMORY_BARRIER, .srcAccessMask = VK_ACCESS_SHADER_WRITE_BIT,
         .dstAccessMask = VK_ACCESS_HOST_READ_BIT };
      vkCmdPipelineBarrier(cbs[k], VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_PIPELINE_STAGE_HOST_BIT,
                           0, 1, &mb, 0, NULL, 0, NULL);
      CHECK(vkEndCommandBuffer(cbs[k]));
      uint64_t sig = (uint64_t)s + 1;
      VkTimelineSemaphoreSubmitInfo tsi = { VK_STRUCTURE_TYPE_TIMELINE_SEMAPHORE_SUBMIT_INFO,
         .signalSemaphoreValueCount = 1, .pSignalSemaphoreValues = &sig };
      VkSubmitInfo si = { VK_STRUCTURE_TYPE_SUBMIT_INFO, &tsi, .commandBufferCount = 1,
         .pCommandBuffers = &cbs[k], .signalSemaphoreCount = 1, .pSignalSemaphores = &timeline };
      CHECK(vkQueueSubmit(q, 1, &si, fences[k]));
      slot_seq[k] = s;
      submitted = s + 1;
   }
   CHECK(vkQueueWaitIdle(q));
   done = 1;
#ifdef _WIN32
   WaitForSingleObject(th, 10000);
#else
   pthread_join(th, NULL);
#endif
   uint32_t final_bad = 0;
   for (uint32_t s = total > 65536 ? total - 65536 : 0; s < total; s++)
      if (results[s % 65536] != s)
         final_bad++;
   printf("fence_bad=%u poll_bad=%u waiter_checks=%u waiter_bad=%u final_missing=%u\n",
          fence_bad, poll_bad, waiter_checks, waiter_bad, final_bad);
   int bad = fence_bad || poll_bad || waiter_bad || final_bad;
   printf("RESULT %s\n", bad ? "FAIL" : "PASS");
   vkDestroyDevice(dev, NULL);
   vkDestroyInstance(inst, NULL);
   return bad ? 2 : 0;
}
