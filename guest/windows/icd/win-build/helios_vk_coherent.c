/*
 * helios_vk_coherent.c - cached HOST_COHERENT contract probe.
 *
 * CPU writes src without vkFlushMappedMemoryRanges, GPU copies src->dst, then
 * CPU reads dst after vkWaitForFences without vkInvalidateMappedMemoryRanges.
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
      if (_r != VK_SUCCESS)                                                    \
         return 10;                                                            \
   } while (0)

static uint32_t
pick_memory_type(VkPhysicalDeviceMemoryProperties *mp,
                 uint32_t type_bits,
                 VkMemoryPropertyFlags required)
{
   for (uint32_t i = 0; i < mp->memoryTypeCount; i++) {
      const VkMemoryPropertyFlags f = mp->memoryTypes[i].propertyFlags;
      if ((type_bits & (1u << i)) && (f & required) == required)
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
      (PFN_vkGetInstanceProcAddr)(void *)GetProcAddress(vk, "vkGetInstanceProcAddr");
   PFN_vkCreateInstance pCreateInstance =
      (PFN_vkCreateInstance)gipa(NULL, "vkCreateInstance");

   const VkApplicationInfo app = {
      .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .pApplicationName = "helios_vk_coherent",
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
   VkPhysicalDevice phys_devs[8];
   if (count > 8)
      count = 8;
   CHECK(pEnum(inst, &count, phys_devs));
   VkPhysicalDevice phys = phys_devs[0];

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

   PFN_vkGetDeviceQueue pGetQueue = DLOAD(vkGetDeviceQueue);
   PFN_vkCreateBuffer pCreateBuffer = DLOAD(vkCreateBuffer);
   PFN_vkGetBufferMemoryRequirements pBufReq = DLOAD(vkGetBufferMemoryRequirements);
   PFN_vkAllocateMemory pAlloc = DLOAD(vkAllocateMemory);
   PFN_vkBindBufferMemory pBind = DLOAD(vkBindBufferMemory);
   PFN_vkMapMemory pMap = DLOAD(vkMapMemory);
   PFN_vkCreateCommandPool pCreatePool = DLOAD(vkCreateCommandPool);
   PFN_vkAllocateCommandBuffers pAllocCmd = DLOAD(vkAllocateCommandBuffers);
   PFN_vkBeginCommandBuffer pBegin = DLOAD(vkBeginCommandBuffer);
   PFN_vkCmdCopyBuffer pCopy = DLOAD(vkCmdCopyBuffer);
   PFN_vkEndCommandBuffer pEnd = DLOAD(vkEndCommandBuffer);
   PFN_vkQueueSubmit pSubmit = DLOAD(vkQueueSubmit);
   PFN_vkCreateFence pCreateFence = DLOAD(vkCreateFence);
   PFN_vkWaitForFences pWait = DLOAD(vkWaitForFences);

   VkQueue queue = VK_NULL_HANDLE;
   pGetQueue(dev, qfi, 0, &queue);

   const VkDeviceSize size = 4096;
   const VkBufferCreateInfo bci = {
      .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
      .size = size,
      .usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT |
               VK_BUFFER_USAGE_TRANSFER_DST_BIT,
      .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
   };
   VkBuffer src = VK_NULL_HANDLE, dst = VK_NULL_HANDLE;
   CHECK(pCreateBuffer(dev, &bci, NULL, &src));
   CHECK(pCreateBuffer(dev, &bci, NULL, &dst));

   VkMemoryRequirements src_req, dst_req;
   pBufReq(dev, src, &src_req);
   pBufReq(dev, dst, &dst_req);
   VkPhysicalDeviceMemoryProperties mp;
   pMemProps(phys, &mp);
   const VkMemoryPropertyFlags req_flags =
      VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
      VK_MEMORY_PROPERTY_HOST_COHERENT_BIT |
      VK_MEMORY_PROPERTY_HOST_CACHED_BIT;
   uint32_t src_type = pick_memory_type(&mp, src_req.memoryTypeBits, req_flags);
   uint32_t dst_type = pick_memory_type(&mp, dst_req.memoryTypeBits, req_flags);
   if (src_type == UINT32_MAX || dst_type == UINT32_MAX) {
      fprintf(stderr, "FAIL: no HOST_VISIBLE|HOST_COHERENT|HOST_CACHED type\n");
      return 5;
   }
   fprintf(stderr, "memory types: src=%u flags=0x%x dst=%u flags=0x%x\n",
           src_type, mp.memoryTypes[src_type].propertyFlags,
           dst_type, mp.memoryTypes[dst_type].propertyFlags);

   const VkMemoryAllocateInfo src_ai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .allocationSize = src_req.size,
      .memoryTypeIndex = src_type,
   };
   const VkMemoryAllocateInfo dst_ai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .allocationSize = dst_req.size,
      .memoryTypeIndex = dst_type,
   };
   VkDeviceMemory src_mem = VK_NULL_HANDLE, dst_mem = VK_NULL_HANDLE;
   CHECK(pAlloc(dev, &src_ai, NULL, &src_mem));
   CHECK(pAlloc(dev, &dst_ai, NULL, &dst_mem));
   CHECK(pBind(dev, src, src_mem, 0));
   CHECK(pBind(dev, dst, dst_mem, 0));

   uint32_t *src_ptr = NULL, *dst_ptr = NULL;
   CHECK(pMap(dev, src_mem, 0, size, 0, (void **)&src_ptr));
   CHECK(pMap(dev, dst_mem, 0, size, 0, (void **)&dst_ptr));
   for (uint32_t i = 0; i < size / 4; i++) {
      src_ptr[i] = 0x12340000u + i;
      dst_ptr[i] = 0xababababu;
   }

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
   const VkBufferCopy copy = { .srcOffset = 0, .dstOffset = 0, .size = size };
   pCopy(cmd, src, dst, 1, &copy);
   CHECK(pEnd(cmd));

   VkFence fence = VK_NULL_HANDLE;
   const VkFenceCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
   CHECK(pCreateFence(dev, &fci, NULL, &fence));
   const VkSubmitInfo si = {
      .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
      .commandBufferCount = 1,
      .pCommandBuffers = &cmd,
   };
   CHECK(pSubmit(queue, 1, &si, fence));
   CHECK(pWait(dev, 1, &fence, VK_TRUE, 5000000000ull));

   for (uint32_t i = 0; i < size / 4; i++) {
      const uint32_t expected = 0x12340000u + i;
      if (dst_ptr[i] != expected) {
         fprintf(stderr, "FAIL: dst[%u]=0x%08x expected=0x%08x\n",
                 i, dst_ptr[i], expected);
         return 6;
      }
   }

   fprintf(stderr, "PASS: cached HOST_COHERENT CPU->GPU and GPU->CPU visibility\n");
   return 0;
}
