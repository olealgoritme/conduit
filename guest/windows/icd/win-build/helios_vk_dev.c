/*
 * helios_vk_dev.c — Phase 5 device + memory bring-up test for the Helios venus
 * ICD. Goes beyond helios_vk_smoke: creates a logical device, allocates a
 * HOST_VISIBLE buffer + memory (exercising vn_renderer bo_ops:
 * create_from_device_memory -> ALLOC_BLOB(blob_id=mem_id), and bo_map ->
 * MAP_BLOB), maps it, writes/reads back a pattern. This is the first end-to-end
 * exercise of the device-memory blob path (only the ring's blob_id=0 shmem was
 * proven by the smoke test).
 *
 * Build (mingw, on win11):
 *   gcc -O2 -o C:\Users\Rupansh\helios_vk_dev.exe Z:\icd\win-build\helios_vk_dev.c -IZ:\icd\mesa\include
 * Run (mingw bin on PATH for libwinpthread; ICD via HKLM registry):
 *   $env:VN_DEBUG="init"; .\helios_vk_dev.exe
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
      fprintf(stderr, "  %-40s => %d %s\n", #expr, _r,                         \
              _r == VK_SUCCESS ? "OK" : "FAIL");                               \
      fflush(stderr);                                                          \
      if (_r != VK_SUCCESS)                                                    \
         return 10;                                                            \
   } while (0)

int
main(void)
{
   HMODULE vk = LoadLibraryW(L"vulkan-1.dll");
   if (!vk) {
      fprintf(stderr, "FAIL: LoadLibrary(vulkan-1.dll)\n");
      return 1;
   }
   PFN_vkGetInstanceProcAddr gipa =
      (PFN_vkGetInstanceProcAddr)(void *)GetProcAddress(vk, "vkGetInstanceProcAddr");

   PFN_vkCreateInstance pCreateInstance =
      (PFN_vkCreateInstance)gipa(NULL, "vkCreateInstance");

   const VkApplicationInfo app = {
      .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .pApplicationName = "helios_vk_dev",
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
   if (count == 0) {
      fprintf(stderr, "FAIL: no physical devices\n");
      return 3;
   }
   VkPhysicalDevice devs[8];
   if (count > 8)
      count = 8;
   CHECK(pEnum(inst, &count, devs));

   /* Pick device 0 (Intel ARL). Switch index to test llvmpipe. */
   VkPhysicalDevice phys = devs[0];
   VkPhysicalDeviceProperties props;
   pProps(phys, &props);
   fprintf(stderr, "Using device 0: \"%s\" (type %d)\n", props.deviceName,
           props.deviceType);

   /* Queue family: pick the first (graphics or compute). */
   uint32_t qfcount = 0;
   pQFP(phys, &qfcount, NULL);
   if (qfcount == 0) {
      fprintf(stderr, "FAIL: no queue families\n");
      return 4;
   }
   VkQueueFamilyProperties qfs[16];
   if (qfcount > 16)
      qfcount = 16;
   pQFP(phys, &qfcount, qfs);
   uint32_t qfi = 0;
   for (uint32_t i = 0; i < qfcount; i++) {
      if (qfs[i].queueFlags & (VK_QUEUE_GRAPHICS_BIT | VK_QUEUE_COMPUTE_BIT)) {
         qfi = i;
         break;
      }
   }
   fprintf(stderr, "Using queue family %u (flags 0x%x)\n", qfi, qfs[qfi].queueFlags);

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
   fprintf(stderr, "[A] vkCreateDevice ...\n");
   fflush(stderr);
   CHECK(pCreateDevice(phys, &dci, NULL, &dev));

   PFN_vkCreateBuffer pCreateBuffer = DLOAD(vkCreateBuffer);
   PFN_vkGetBufferMemoryRequirements pBufReq = DLOAD(vkGetBufferMemoryRequirements);
   PFN_vkAllocateMemory pAlloc = DLOAD(vkAllocateMemory);
   PFN_vkBindBufferMemory pBind = DLOAD(vkBindBufferMemory);
   PFN_vkMapMemory pMap = DLOAD(vkMapMemory);
   PFN_vkUnmapMemory pUnmap = DLOAD(vkUnmapMemory);
   PFN_vkFreeMemory pFree = DLOAD(vkFreeMemory);
   PFN_vkDestroyBuffer pDestroyBuffer = DLOAD(vkDestroyBuffer);
   PFN_vkDestroyDevice pDestroyDevice = DLOAD(vkDestroyDevice);

   const VkDeviceSize bufsize = 64 * 1024;
   const VkBufferCreateInfo bci = {
      .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
      .size = bufsize,
      .usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT | VK_BUFFER_USAGE_TRANSFER_DST_BIT,
      .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
   };
   VkBuffer buf = VK_NULL_HANDLE;
   fprintf(stderr, "[B] vkCreateBuffer ...\n");
   fflush(stderr);
   CHECK(pCreateBuffer(dev, &bci, NULL, &buf));

   VkMemoryRequirements req;
   pBufReq(dev, buf, &req);
   fprintf(stderr, "  mem req: size=%llu align=%llu typeBits=0x%x\n",
           (unsigned long long)req.size, (unsigned long long)req.alignment,
           req.memoryTypeBits);

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
      fprintf(stderr, "FAIL: no HOST_VISIBLE|COHERENT memory type\n");
      return 5;
   }
   fprintf(stderr, "Using memory type %u\n", mti);

   const VkMemoryAllocateInfo mai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .allocationSize = req.size,
      .memoryTypeIndex = mti,
   };
   VkDeviceMemory mem = VK_NULL_HANDLE;
   fprintf(stderr, "[C] vkAllocateMemory (HOST_VISIBLE -> ALLOC_BLOB blob_id=mem_id) ...\n");
   fflush(stderr);
   CHECK(pAlloc(dev, &mai, NULL, &mem));
   CHECK(pBind(dev, buf, mem, 0));

   void *ptr = NULL;
   fprintf(stderr, "[D] vkMapMemory (-> MAP_BLOB) ...\n");
   fflush(stderr);
   CHECK(pMap(dev, mem, 0, bufsize, 0, &ptr));
   fprintf(stderr, "  mapped at %p\n", ptr);
   fflush(stderr);

   /* Write a pattern, read it back. */
   uint32_t *u = (uint32_t *)ptr;
   for (uint32_t i = 0; i < 1024; i++)
      u[i] = 0xC0FFEE00u ^ i;
   int ok = 1;
   for (uint32_t i = 0; i < 1024; i++) {
      if (u[i] != (0xC0FFEE00u ^ i)) {
         ok = 0;
         break;
      }
   }
   fprintf(stderr, "[E] map readback %s\n", ok ? "OK" : "MISMATCH");
   fflush(stderr);

   pUnmap(dev, mem);
   pDestroyBuffer(dev, buf, NULL);
   pFree(dev, mem, NULL);
   pDestroyDevice(dev, NULL);

   fprintf(stderr, "PASS: device + host-visible memory map round-trip (bo_ops)%s\n",
           ok ? "" : " [readback mismatch]");
   return ok ? 0 : 6;
}
