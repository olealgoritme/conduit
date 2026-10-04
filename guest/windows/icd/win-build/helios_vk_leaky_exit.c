/*
 * helios_vk_leaky_exit.c - teardown probe for mapped HOST_COHERENT memory.
 *
 * This intentionally leaves a mapped allocation live across vkDestroyDevice.
 * The app behavior is not Vulkan-clean, but the ICD must not trip an internal
 * debug assertion while tearing down its own tracking state.
 */
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <stdint.h>
#include <stdio.h>

#define VK_NO_PROTOTYPES
#include <vulkan/vulkan.h>

#define ILOAD(name) (PFN_##name)(void *) gipa(inst, #name)
#define DLOAD(name) (PFN_##name)(void *) gdpa(dev, #name)

#define CHECK(expr)                                                            \
   do {                                                                        \
      VkResult _r = (expr);                                                    \
      fprintf(stderr, "  %-44s => %d %s\n", #expr, _r,                         \
              _r == VK_SUCCESS ? "OK" : "FAIL");                               \
      if (_r != VK_SUCCESS)                                                    \
         return 10;                                                            \
   } while (0)

static uint32_t
pick_memory_type(const VkPhysicalDeviceMemoryProperties *mp,
                 uint32_t type_bits,
                 VkMemoryPropertyFlags required)
{
   for (uint32_t i = 0; i < mp->memoryTypeCount; i++) {
      const VkMemoryPropertyFlags flags = mp->memoryTypes[i].propertyFlags;
      if ((type_bits & (1u << i)) && (flags & required) == required)
         return i;
   }

   return UINT32_MAX;
}

int
main(void)
{
   HMODULE vk = LoadLibraryW(L"vulkan-1.dll");
   if (!vk)
      return 1;

   PFN_vkGetInstanceProcAddr gipa =
      (PFN_vkGetInstanceProcAddr)(void *)GetProcAddress(vk,
                                                        "vkGetInstanceProcAddr");
   PFN_vkCreateInstance pCreateInstance =
      (PFN_vkCreateInstance)gipa(NULL, "vkCreateInstance");

   const VkApplicationInfo app = {
      .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .pApplicationName = "helios_vk_leaky_exit",
      .apiVersion = VK_API_VERSION_1_1,
   };
   const VkInstanceCreateInfo ici = {
      .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
      .pApplicationInfo = &app,
   };
   VkInstance inst = VK_NULL_HANDLE;
   CHECK(pCreateInstance(&ici, NULL, &inst));

   PFN_vkEnumeratePhysicalDevices pEnum = ILOAD(vkEnumeratePhysicalDevices);
   PFN_vkGetPhysicalDeviceQueueFamilyProperties pQFP =
      ILOAD(vkGetPhysicalDeviceQueueFamilyProperties);
   PFN_vkGetPhysicalDeviceMemoryProperties pMemProps =
      ILOAD(vkGetPhysicalDeviceMemoryProperties);
   PFN_vkCreateDevice pCreateDevice = ILOAD(vkCreateDevice);
   PFN_vkDestroyInstance pDestroyInstance = ILOAD(vkDestroyInstance);
   PFN_vkGetDeviceProcAddr gdpa = ILOAD(vkGetDeviceProcAddr);

   uint32_t count = 0;
   CHECK(pEnum(inst, &count, NULL));
   VkPhysicalDevice phys_devs[8];
   if (count > 8)
      count = 8;
   CHECK(pEnum(inst, &count, phys_devs));
   VkPhysicalDevice phys = phys_devs[0];

   uint32_t qfcount = 0;
   pQFP(phys, &qfcount, NULL);
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

   PFN_vkCreateBuffer pCreateBuffer = DLOAD(vkCreateBuffer);
   PFN_vkGetBufferMemoryRequirements pBufReq =
      DLOAD(vkGetBufferMemoryRequirements);
   PFN_vkAllocateMemory pAlloc = DLOAD(vkAllocateMemory);
   PFN_vkBindBufferMemory pBind = DLOAD(vkBindBufferMemory);
   PFN_vkMapMemory pMap = DLOAD(vkMapMemory);
   PFN_vkDestroyDevice pDestroyDevice = DLOAD(vkDestroyDevice);

   const VkDeviceSize size = 4096;
   const VkBufferCreateInfo bci = {
      .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
      .size = size,
      .usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT,
      .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
   };
   VkBuffer buf = VK_NULL_HANDLE;
   CHECK(pCreateBuffer(dev, &bci, NULL, &buf));

   VkMemoryRequirements req;
   pBufReq(dev, buf, &req);
   VkPhysicalDeviceMemoryProperties mp;
   pMemProps(phys, &mp);
   const VkMemoryPropertyFlags required =
      VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
      VK_MEMORY_PROPERTY_HOST_COHERENT_BIT |
      VK_MEMORY_PROPERTY_HOST_CACHED_BIT;
   const uint32_t mem_type =
      pick_memory_type(&mp, req.memoryTypeBits, required);
   if (mem_type == UINT32_MAX) {
      fprintf(stderr, "FAIL: no cached coherent memory type\n");
      return 5;
   }

   VkDeviceMemory mem = VK_NULL_HANDLE;
   const VkMemoryAllocateInfo ai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .allocationSize = req.size,
      .memoryTypeIndex = mem_type,
   };
   CHECK(pAlloc(dev, &ai, NULL, &mem));
   CHECK(pBind(dev, buf, mem, 0));

   void *ptr = NULL;
   CHECK(pMap(dev, mem, 0, size, 0, &ptr));
   ((uint32_t *)ptr)[0] = 0xfeedbeefu;

   pDestroyDevice(dev, NULL);
   pDestroyInstance(inst, NULL);
   fprintf(stderr, "PASS: mapped coherent memory teardown did not assert\n");
   return 0;
}
