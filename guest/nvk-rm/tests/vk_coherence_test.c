/* CPU <-> GPU coherence of host-visible memory, repeated.
 *
 * For every HOST_VISIBLE | HOST_COHERENT memory type: a persistently mapped
 * storage buffer, and per iteration
 *   1. the CPU writes a fresh pattern to all of it,
 *   2. a compute dispatch reads and rewrites it (data = data * mul + 7),
 *   3. the CPU reads it back and checks every value,
 *   4. the CPU rewrites only the even elements (so lines hold CPU and GPU
 *      data side by side), a second dispatch with another mul, CPU check.
 * The pattern and mul change every iteration, so a stale line anywhere
 * (GPU L2 holding an older CPU write, or a dirty GPU line not written back
 * before the fence) shows up as a mismatch.  This is the check for
 * patch 27 (system memory mapped GPU-cacheable, L2 sysmem invalidate per
 * submit), whose mapping flags go through the Windows KMD.
 *
 *   vk_coherence_test bar.comp.spv [iters [n]]     (default 500, 1M values)
 *   vk_coherence_test bar.comp.spv load SECONDS    (GPU load to run alongside:
 *                                                    compute on a 32 MiB
 *                                                    host-visible buffer)
 * Device-local host-visible types (the BAR heap, slow CPU reads) use n/64.
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

static inline uint32_t hash(uint32_t x)
{
   x ^= x >> 16; x *= 0x7feb352du;
   x ^= x >> 15; x *= 0x846ca68bu;
   x ^= x >> 16;
   return x;
}

static VkDevice dev;
static VkQueue q;
static VkPipeline pipe;
static VkPipelineLayout pl;
static VkDescriptorSetLayout dsl;
static VkDescriptorPool dp;
static VkCommandPool cp;
static VkCommandBuffer cb;
static VkFence fence;
static VkPhysicalDeviceMemoryProperties mp;

struct target {
   VkBuffer buf;
   VkDeviceMemory mem;
   VkDescriptorSet ds;
   uint32_t *ptr;
   uint32_t n;
};

static void make_target(struct target *t, uint32_t type, uint32_t n)
{
   VkBufferCreateInfo bci = { .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
      .size = (VkDeviceSize)n * 4, .usage = VK_BUFFER_USAGE_STORAGE_BUFFER_BIT,
      .sharingMode = VK_SHARING_MODE_EXCLUSIVE };
   CHECK(vkCreateBuffer(dev, &bci, NULL, &t->buf));
   VkMemoryRequirements mr;
   vkGetBufferMemoryRequirements(dev, t->buf, &mr);
   if (!(mr.memoryTypeBits & (1u << type))) {
      printf("FAIL: storage buffer cannot live in type %u\n", type);
      exit(1);
   }
   VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .allocationSize = mr.size, .memoryTypeIndex = type };
   CHECK(vkAllocateMemory(dev, &mai, NULL, &t->mem));
   CHECK(vkBindBufferMemory(dev, t->buf, t->mem, 0));
   CHECK(vkMapMemory(dev, t->mem, 0, VK_WHOLE_SIZE, 0, (void **)&t->ptr));
   VkDescriptorSetAllocateInfo dsai = { .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
      .descriptorPool = dp, .descriptorSetCount = 1, .pSetLayouts = &dsl };
   CHECK(vkAllocateDescriptorSets(dev, &dsai, &t->ds));
   VkDescriptorBufferInfo dbi = { t->buf, 0, VK_WHOLE_SIZE };
   VkWriteDescriptorSet w = { .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = t->ds,
      .dstBinding = 0, .descriptorCount = 1,
      .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, .pBufferInfo = &dbi };
   vkUpdateDescriptorSets(dev, 1, &w, 0, NULL);
   t->n = n;
}

static void free_target(struct target *t)
{
   vkUnmapMemory(dev, t->mem);
   vkDestroyBuffer(dev, t->buf, NULL);
   vkFreeMemory(dev, t->mem, NULL);
}

/* reps dispatches of data = data * mul + 7, host barriers around them */
static void run(struct target *t, uint32_t mul, int reps, int wait)
{
   CHECK(vkResetCommandPool(dev, cp, 0));
   VkCommandBufferBeginInfo cbbi = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
      .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT };
   CHECK(vkBeginCommandBuffer(cb, &cbbi));
   VkMemoryBarrier hb = { .sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
      .srcAccessMask = VK_ACCESS_HOST_WRITE_BIT,
      .dstAccessMask = VK_ACCESS_SHADER_READ_BIT | VK_ACCESS_SHADER_WRITE_BIT };
   vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_HOST_BIT, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
                        0, 1, &hb, 0, NULL, 0, NULL);
   vkCmdBindPipeline(cb, VK_PIPELINE_BIND_POINT_COMPUTE, pipe);
   vkCmdBindDescriptorSets(cb, VK_PIPELINE_BIND_POINT_COMPUTE, pl, 0, 1, &t->ds, 0, NULL);
   uint32_t pc[2] = { t->n, mul };
   vkCmdPushConstants(cb, pl, VK_SHADER_STAGE_COMPUTE_BIT, 0, 8, pc);
   for (int r = 0; r < reps; r++) {
      if (r) {
         VkMemoryBarrier cc = { .sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
            .srcAccessMask = VK_ACCESS_SHADER_WRITE_BIT,
            .dstAccessMask = VK_ACCESS_SHADER_READ_BIT | VK_ACCESS_SHADER_WRITE_BIT };
         vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
                              VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, 0, 1, &cc, 0, NULL, 0, NULL);
      }
      vkCmdDispatch(cb, (t->n + 63) / 64, 1, 1);
   }
   VkMemoryBarrier mb = { .sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
      .srcAccessMask = VK_ACCESS_SHADER_WRITE_BIT, .dstAccessMask = VK_ACCESS_HOST_READ_BIT };
   vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_PIPELINE_STAGE_HOST_BIT,
                        0, 1, &mb, 0, NULL, 0, NULL);
   CHECK(vkEndCommandBuffer(cb));
   VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1,
      .pCommandBuffers = &cb };
   CHECK(vkResetFences(dev, 1, &fence));
   CHECK(vkQueueSubmit(q, 1, &si, fence));
   if (wait)
      CHECK(vkWaitForFences(dev, 1, &fence, VK_TRUE, 10ull * 1000 * 1000 * 1000));
}

static uint64_t verify(const uint32_t *ptr, const uint32_t *want, uint32_t n,
                       uint32_t iter, const char *step, uint64_t *shown)
{
   uint64_t bad = 0;
   for (uint32_t i = 0; i < n; i++) {
      const uint32_t v = ptr[i];
      if (v != want[i]) {
         if ((*shown)++ < 10)
            printf("  iter %u %s: mismatch at %u: got 0x%08x want 0x%08x\n",
                   iter, step, i, v, want[i]);
         bad++;
      }
   }
   return bad;
}

int main(int argc, char **argv)
{
   const char *spv_path = argc > 1 ? argv[1] : "bar.comp.spv";
   const int load = argc > 2 && !strcmp(argv[2], "load");
   const double load_s = load && argc > 3 ? atof(argv[3]) : 30.0;
   const uint32_t iters = !load && argc > 2 ? (uint32_t)atoi(argv[2]) : 500;
   const uint32_t n = !load && argc > 3 ? (uint32_t)atoi(argv[3]) : (1u << 20);

   VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .pApplicationName = "vk_coherence_test", .apiVersion = VK_API_VERSION_1_3 };
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
   printf("device: %s%s\n", props.deviceName, load ? " (load)" : "");
   vkGetPhysicalDeviceMemoryProperties(pd, &mp);

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
   CHECK(vkCreateDevice(pd, &dci, NULL, &dev));
   vkGetDeviceQueue(dev, qf, 0, &q);

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
   CHECK(vkCreateDescriptorSetLayout(dev, &dslci, NULL, &dsl));
   VkPushConstantRange pcr = { .stageFlags = VK_SHADER_STAGE_COMPUTE_BIT, .offset = 0, .size = 8 };
   VkPipelineLayoutCreateInfo plci = { .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
      .setLayoutCount = 1, .pSetLayouts = &dsl, .pushConstantRangeCount = 1,
      .pPushConstantRanges = &pcr };
   CHECK(vkCreatePipelineLayout(dev, &plci, NULL, &pl));
   VkComputePipelineCreateInfo cpci = { .sType = VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO,
      .stage = { .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
                 .stage = VK_SHADER_STAGE_COMPUTE_BIT, .module = sm, .pName = "main" },
      .layout = pl };
   CHECK(vkCreateComputePipelines(dev, VK_NULL_HANDLE, 1, &cpci, NULL, &pipe));
   VkDescriptorPoolSize ps = { VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 32 };
   VkDescriptorPoolCreateInfo dpci = { .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
      .maxSets = 32, .poolSizeCount = 1, .pPoolSizes = &ps };
   CHECK(vkCreateDescriptorPool(dev, &dpci, NULL, &dp));
   VkCommandPoolCreateInfo cpi = { .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
      .queueFamilyIndex = qf };
   CHECK(vkCreateCommandPool(dev, &cpi, NULL, &cp));
   VkCommandBufferAllocateInfo cbai = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
      .commandPool = cp, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1 };
   CHECK(vkAllocateCommandBuffers(dev, &cbai, &cb));
   VkFenceCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
   CHECK(vkCreateFence(dev, &fci, NULL, &fence));

   const VkMemoryPropertyFlags hvc = VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                                     VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;
   if (load) {
      /* Fill L2 with dirty system-memory lines of another process */
      uint32_t type = UINT32_MAX;
      for (uint32_t i = 0; i < mp.memoryTypeCount && type == UINT32_MAX; i++)
         if ((mp.memoryTypes[i].propertyFlags & hvc) == hvc &&
             !(mp.memoryTypes[i].propertyFlags & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT))
            type = i;
      if (type == UINT32_MAX) { printf("FAIL: no host-visible system memory type\n"); return 1; }
      struct target t;
      make_target(&t, type, 8u << 20);
      memset(t.ptr, 0, (size_t)t.n * 4);
      double t0 = now_s(), t1 = t0;
      unsigned submits = 0;
      while ((t1 = now_s()) - t0 < load_s) {
         run(&t, 5, 40, 1);
         submits++;
      }
      printf("load: %u submits of 40 dispatches over 32 MiB (type %u) in %.1f s\n",
             submits, type, t1 - t0);
      free_target(&t);
      return 0;
   }

   uint64_t total_bad = 0;
   uint32_t tested = 0;
   for (uint32_t type = 0; type < mp.memoryTypeCount; type++) {
      const VkMemoryPropertyFlags f = mp.memoryTypes[type].propertyFlags;
      if ((f & hvc) != hvc)
         continue;
      const int vram = !!(f & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT);
      const uint32_t tn = vram ? (n / 64 ? n / 64 : 1) : n;
      struct target t;
      make_target(&t, type, tn);
      uint32_t *want = malloc((size_t)tn * 4);
      uint64_t bad = 0, shown = 0;
      double t0 = now_s();
      for (uint32_t it = 0; it < iters; it++) {
         const uint32_t seed = hash(it * 2654435761u + type);
         const uint32_t mul1 = 3 + 2 * (it % 7), mul2 = 5 + 2 * (it % 11);
         /* 1-3: full CPU write, GPU read+write (1-3 dispatches), CPU check */
         const int reps = 1 + it % 3;
         for (uint32_t i = 0; i < tn; i++) {
            const uint32_t v = hash(i ^ seed);
            t.ptr[i] = v;
            uint32_t w = v;
            for (int r = 0; r < reps; r++)
               w = w * mul1 + 7u;
            want[i] = w;
         }
         run(&t, mul1, reps, 1);
         bad += verify(t.ptr, want, tn, it, "full", &shown);
         /* 4: CPU rewrites the even elements only, GPU again, CPU check */
         for (uint32_t i = 0; i < tn; i++) {
            uint32_t v = want[i];
            if (!(i & 1)) {
               v = hash(i + seed);
               t.ptr[i] = v;
            }
            want[i] = v * mul2 + 7u;
         }
         run(&t, mul2, 1, 1);
         bad += verify(t.ptr, want, tn, it, "even", &shown);
      }
      double t1 = now_s();
      printf("%s: type %u (flags 0x%x, heap %u%s): %u iterations x 2 round trips of %u values, "
             "%llu bad, %.2f ms per iteration\n", bad ? "FAIL" : "PASS", type, f,
             mp.memoryTypes[type].heapIndex, vram ? ", VRAM" : ", system memory",
             iters, tn, (unsigned long long)bad, (t1 - t0) * 1e3 / iters);
      total_bad += bad;
      tested++;
      free(want);
      free_target(&t);
   }
   if (!tested)
      printf("FAIL: no HOST_VISIBLE | HOST_COHERENT memory type\n");
   printf("%s: CPU <-> GPU coherence, %u memory type(s)\n",
          (total_bad || !tested) ? "FAIL" : "PASS", tested);

   vkDestroyFence(dev, fence, NULL);
   vkDestroyCommandPool(dev, cp, NULL);
   vkDestroyDescriptorPool(dev, dp, NULL);
   vkDestroyPipeline(dev, pipe, NULL);
   vkDestroyPipelineLayout(dev, pl, NULL);
   vkDestroyDescriptorSetLayout(dev, dsl, NULL);
   vkDestroyShaderModule(dev, sm, NULL);
   vkDestroyDevice(dev, NULL);
   vkDestroyInstance(inst, NULL);
   return (total_bad || !tested) ? 1 : 0;
}
