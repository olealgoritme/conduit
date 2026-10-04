/*
 * helios_vk_exec.c — Phase 5 GPU-execution test for the Helios venus ICD.
 * Beyond helios_vk_dev (which only allocated+mapped memory), this records a
 * command buffer that runs a real GPU transfer (vkCmdFillBuffer), submits it on
 * a queue with a fence, waits, then maps the buffer and verifies the GPU wrote
 * the fill pattern. This exercises the full execution+sync path end-to-end:
 * command pool/buffer, vkQueueSubmit (-> venus -> host queue submit), fence
 * signalling (vkWaitForFences), and host-visible readback of GPU output.
 *
 * Build (mingw, on win11):
 *   gcc -O2 -o C:\Users\Rupansh\helios_vk_exec.exe Z:\icd\win-build\helios_vk_exec.c -IZ:\icd\mesa\include
 * Run: $env:VN_DEBUG="init"; .\helios_vk_exec.exe
 */
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <stdint.h>
#include <stdio.h>
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
   HMODULE vk = LoadLibraryW(L"vulkan-1.dll");
   if (!vk)
      return 1;
   PFN_vkGetInstanceProcAddr gipa =
      (PFN_vkGetInstanceProcAddr)(void *)GetProcAddress(vk, "vkGetInstanceProcAddr");
   PFN_vkCreateInstance pCreateInstance =
      (PFN_vkCreateInstance)gipa(NULL, "vkCreateInstance");

   const VkApplicationInfo app = {
      .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .pApplicationName = "helios_vk_exec",
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
   VkPhysicalDevice phys = devs[0];
   VkPhysicalDeviceProperties props;
   pProps(phys, &props);
   fprintf(stderr, "Using device 0: \"%s\"\n", props.deviceName);

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

   /* Map and pre-fill with a sentinel so we can confirm the GPU overwrote it. */
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
   /* The actual GPU work: fill the whole buffer with 0xDEADBEEF. */
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
   CHECK(pWait(dev, 1, &fence, VK_TRUE, 5000000000ull /* 5s */));
   {
      VkResult wi = pQWaitIdle(queue);
      fprintf(stderr, "  vkQueueWaitIdle => %d %s (non-fatal; fence already waited)\n",
              wi, wi == VK_SUCCESS ? "OK" : "ERR");
      fflush(stderr);
   }

   /* Verify the GPU wrote the fill pattern over our 0xAB sentinel. */
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
      fprintf(stderr, "    first mismatch at word %u = 0x%08x\n", firstbad, u[firstbad]);
   fflush(stderr);

   fprintf(stderr, "%s: GPU command execution (fill+submit+fence+readback)\n",
           ok ? "PASS" : "FAIL");
   return ok ? 0 : 6;
}
