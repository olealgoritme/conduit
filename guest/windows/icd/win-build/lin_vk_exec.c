/*
 * lin_vk_exec.c — Linux port of helios_vk_exec.c, for the WORKING venus
 * reference (Ubuntu VM, real virtgpu). Same flow: vkCmdFillBuffer(0xDEADBEEF) ->
 * vkQueueSubmit + fence -> vkWaitForFences -> vkQueueWaitIdle -> readback. The
 * point is the `vkQueueWaitIdle => ?` line: if it returns 0 OK on real venus,
 * the Helios -13 (ffb-not-signaled) is a Helios-specific bug, not venus
 * behaviour — so VN_PERF=no_fence_feedback would be masking it.
 *
 * Build: gcc -O2 -o lin_vk_exec lin_vk_exec.c -ldl
 * Run:   VN_DEBUG=init ./lin_vk_exec
 */
#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define VK_NO_PROTOTYPES
#include <vulkan/vulkan.h>

#define ILOAD(name) (PFN_##name)(void *) gipa(inst, #name)
#define DLOAD(name) (PFN_##name)(void *) gdpa(dev, #name)

#define CHECK(expr)                                                            \
   do {                                                                        \
      VkResult _r = (expr);                                                    \
      fprintf(stderr, "  %-44s => %d %s\n", #expr, _r,                         \
              _r == VK_SUCCESS ? "OK" : "FAIL");                               \
      fflush(stderr);                                                          \
      if (_r != VK_SUCCESS)                                                    \
         return 10;                                                            \
   } while (0)

int
main(void)
{
   void *vk = dlopen("libvulkan.so.1", RTLD_NOW | RTLD_LOCAL);
   if (!vk)
      vk = dlopen("libvulkan.so", RTLD_NOW | RTLD_LOCAL);
   if (!vk) {
      fprintf(stderr, "dlopen libvulkan failed: %s\n", dlerror());
      return 1;
   }
   PFN_vkGetInstanceProcAddr gipa =
      (PFN_vkGetInstanceProcAddr)(void *)dlsym(vk, "vkGetInstanceProcAddr");
   PFN_vkCreateInstance pCreateInstance =
      (PFN_vkCreateInstance)gipa(NULL, "vkCreateInstance");

   const VkApplicationInfo app = {
      .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .pApplicationName = "lin_vk_exec",
      .apiVersion = VK_API_VERSION_1_1,
   };
   const VkInstanceCreateInfo ici = {
      .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
      .pApplicationInfo = &app,
   };
   VkInstance inst = VK_NULL_HANDLE;
   CHECK(pCreateInstance(&ici, NULL, &inst));

   PFN_vkEnumeratePhysicalDevices pEnum = ILOAD(vkEnumeratePhysicalDevices);
   PFN_vkGetPhysicalDeviceProperties pProps = ILOAD(vkGetPhysicalDeviceProperties);
   PFN_vkGetPhysicalDeviceQueueFamilyProperties pQFP =
      ILOAD(vkGetPhysicalDeviceQueueFamilyProperties);
   PFN_vkGetPhysicalDeviceMemoryProperties pMemProps =
      ILOAD(vkGetPhysicalDeviceMemoryProperties);
   PFN_vkCreateDevice pCreateDevice = ILOAD(vkCreateDevice);
   PFN_vkGetDeviceProcAddr gdpa = ILOAD(vkGetDeviceProcAddr);

   uint32_t count = 0;
   CHECK(pEnum(inst, &count, NULL));
   VkPhysicalDevice devs[8];
   if (count > 8)
      count = 8;
   CHECK(pEnum(inst, &count, devs));
   /* Prefer a venus device (driverName "venus"); fall back to devs[0]. */
   VkPhysicalDevice phys = devs[0];
   VkPhysicalDeviceProperties props;
   /* HELIOS_DEV_SUBSTR picks the device whose name contains it (default "ARL" =
    * the Intel iGPU venus device, the same host GPU Helios uses). */
   const char *want = getenv("HELIOS_DEV_SUBSTR");
   if (!want)
      want = "ARL";
   for (uint32_t i = 0; i < count; i++) {
      pProps(devs[i], &props);
      fprintf(stderr, "  dev[%u]: \"%s\"\n", i, props.deviceName);
      if (strstr(props.deviceName, want))
         phys = devs[i];
   }
   pProps(phys, &props);
   fprintf(stderr, "Using device: \"%s\"\n", props.deviceName);

   uint32_t qfcount = 0;
   pQFP(phys, &qfcount, NULL);
   VkQueueFamilyProperties qfs[16];
   if (qfcount > 16)
      qfcount = 16;
   pQFP(phys, &qfcount, qfs);
   uint32_t qfi = 0;
   for (uint32_t i = 0; i < qfcount; i++)
      if (qfs[i].queueFlags & (VK_QUEUE_GRAPHICS_BIT | VK_QUEUE_COMPUTE_BIT)) {
         qfi = i;
         break;
      }

   const float prio = 1.0f;
   const VkDeviceQueueCreateInfo qci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = qfi,
      .queueCount = 1,
      .pQueuePriorities = &prio,
   };
   const VkDeviceCreateInfo dci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
      .queueCreateInfoCount = 1,
      .pQueueCreateInfos = &qci,
   };
   VkDevice dev = VK_NULL_HANDLE;
   CHECK(pCreateDevice(phys, &dci, NULL, &dev));

   PFN_vkGetDeviceQueue pGetQueue = DLOAD(vkGetDeviceQueue);
   PFN_vkCreateBuffer pCreateBuffer = DLOAD(vkCreateBuffer);
   PFN_vkGetBufferMemoryRequirements pBufReq = DLOAD(vkGetBufferMemoryRequirements);
   PFN_vkAllocateMemory pAlloc = DLOAD(vkAllocateMemory);
   PFN_vkBindBufferMemory pBind = DLOAD(vkBindBufferMemory);
   PFN_vkMapMemory pMap = DLOAD(vkMapMemory);
   PFN_vkCreateCommandPool pCreatePool = DLOAD(vkCreateCommandPool);
   PFN_vkAllocateCommandBuffers pAllocCmd = DLOAD(vkAllocateCommandBuffers);
   PFN_vkBeginCommandBuffer pBegin = DLOAD(vkBeginCommandBuffer);
   PFN_vkCmdFillBuffer pFill = DLOAD(vkCmdFillBuffer);
   PFN_vkEndCommandBuffer pEnd = DLOAD(vkEndCommandBuffer);
   PFN_vkQueueSubmit pSubmit = DLOAD(vkQueueSubmit);
   PFN_vkCreateFence pCreateFence = DLOAD(vkCreateFence);
   PFN_vkWaitForFences pWait = DLOAD(vkWaitForFences);
   PFN_vkQueueWaitIdle pQWaitIdle = DLOAD(vkQueueWaitIdle);

   VkQueue queue = VK_NULL_HANDLE;
   pGetQueue(dev, qfi, 0, &queue);

   const VkDeviceSize bufsize = 4096;
   const VkBufferCreateInfo bci = {
      .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
      .size = bufsize,
      .usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT,
      .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
   };
   VkBuffer buf = VK_NULL_HANDLE;
   CHECK(pCreateBuffer(dev, &bci, NULL, &buf));

   VkMemoryRequirements req;
   pBufReq(dev, buf, &req);
   VkPhysicalDeviceMemoryProperties mp;
   pMemProps(phys, &mp);
   uint32_t mti = UINT32_MAX;
   for (uint32_t i = 0; i < mp.memoryTypeCount; i++) {
      const VkMemoryPropertyFlags f = mp.memoryTypes[i].propertyFlags;
      if ((req.memoryTypeBits & (1u << i)) &&
          (f & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT) &&
          (f & VK_MEMORY_PROPERTY_HOST_COHERENT_BIT)) {
         mti = i;
         break;
      }
   }
   if (mti == UINT32_MAX) {
      fprintf(stderr, "FAIL: no HOST_VISIBLE|COHERENT memory\n");
      return 5;
   }
   const VkMemoryAllocateInfo mai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .allocationSize = req.size,
      .memoryTypeIndex = mti,
   };
   VkDeviceMemory mem = VK_NULL_HANDLE;
   CHECK(pAlloc(dev, &mai, NULL, &mem));
   CHECK(pBind(dev, buf, mem, 0));

   void *ptr = NULL;
   CHECK(pMap(dev, mem, 0, bufsize, 0, &ptr));
   memset(ptr, 0xAB, bufsize);

   VkCommandPool pool = VK_NULL_HANDLE;
   const VkCommandPoolCreateInfo pci = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
      .queueFamilyIndex = qfi,
   };
   CHECK(pCreatePool(dev, &pci, NULL, &pool));

   VkCommandBuffer cmd = VK_NULL_HANDLE;
   const VkCommandBufferAllocateInfo cbai = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
      .commandPool = pool,
      .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
      .commandBufferCount = 1,
   };
   CHECK(pAllocCmd(dev, &cbai, &cmd));

   const VkCommandBufferBeginInfo cbi = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
      .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
   };
   CHECK(pBegin(cmd, &cbi));
   pFill(cmd, buf, 0, bufsize, 0xDEADBEEFu);
   CHECK(pEnd(cmd));

   VkFence fence = VK_NULL_HANDLE;
   const VkFenceCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
   CHECK(pCreateFence(dev, &fci, NULL, &fence));

   const VkSubmitInfo si = {
      .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
      .commandBufferCount = 1,
      .pCommandBuffers = &cmd,
   };
   fprintf(stderr, "[X] vkQueueSubmit (vkCmdFillBuffer 0xDEADBEEF) ...\n");
   fflush(stderr);
   CHECK(pSubmit(queue, 1, &si, fence));

   fprintf(stderr, "[Y] vkWaitForFences ...\n");
   fflush(stderr);
   CHECK(pWait(dev, 1, &fence, VK_TRUE, 5000000000ull));
   {
      VkResult wi = pQWaitIdle(queue);
      fprintf(stderr, "  vkQueueWaitIdle => %d %s\n", wi,
              wi == VK_SUCCESS ? "OK" : "ERR");
      fflush(stderr);
   }

   const uint32_t *u = (const uint32_t *)ptr;
   int ok = 1;
   uint32_t firstbad = 0;
   for (uint32_t i = 0; i < bufsize / 4; i++) {
      if (u[i] != 0xDEADBEEFu) {
         ok = 0;
         firstbad = i;
         break;
      }
   }
   fprintf(stderr, "[Z] readback: u[0]=0x%08x %s\n", u[0],
           ok ? "ALL 0xDEADBEEF" : "MISMATCH");
   if (!ok)
      fprintf(stderr, "    first mismatch at word %u = 0x%08x\n", firstbad,
              u[firstbad]);
   fflush(stderr);

   fprintf(stderr, "%s: GPU command execution (fill+submit+fence+readback)\n",
           ok ? "PASS" : "FAIL");
   return ok ? 0 : 6;
}
