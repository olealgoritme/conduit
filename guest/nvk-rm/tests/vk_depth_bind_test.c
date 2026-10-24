/* Depth/stencil image bindings with dedicated and suballocated memory. */
#define VK_NO_PROTOTYPES
#include <windows.h>
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "vk_direct_driver.h"

#define CHECK(x) do { VkResult r_ = (x); if (r_ != VK_SUCCESS) { \
   fprintf(stderr, "FAIL %s:%d: %s -> %d\n", __FILE__, __LINE__, #x, r_); exit(1); } } while (0)
#define LOAD(name) PFN_##name name = (PFN_##name)get(inst, #name)

static const struct {
   VkFormat format;
   const char *name;
} formats[] = {
   { VK_FORMAT_D32_SFLOAT_S8_UINT, "D32S8" },
   { VK_FORMAT_D32_SFLOAT, "D32" },
   { VK_FORMAT_D24_UNORM_S8_UINT, "D24S8" },
   { VK_FORMAT_D16_UNORM, "D16" },
   { VK_FORMAT_S8_UINT, "S8" },
   { VK_FORMAT_R8G8B8A8_UNORM, "RGBA8" },
};
static const struct {
   uint32_t width, height, layers, levels;
} shapes[] = {
   { 1, 1, 1, 1 }, { 63, 65, 1, 1 }, { 128, 128, 1, 1 },
   { 959, 539, 1, 1 }, { 960, 540, 1, 1 }, { 961, 541, 1, 1 },
   { 1920, 1080, 1, 1 }, { 257, 129, 3, 4 },
};

int main(void)
{
   setvbuf(stdout, NULL, _IOLBF, 0);
   HMODULE loader = LoadLibraryA("vulkan-1.dll");
   if (!loader)
      return 1;
   PFN_vkGetInstanceProcAddr get = (PFN_vkGetInstanceProcAddr)(void *)
      GetProcAddress(loader, "vkGetInstanceProcAddr");
   PFN_vkCreateInstance create = (PFN_vkCreateInstance)get(NULL, "vkCreateInstance");
   VkApplicationInfo app = { VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .pApplicationName = "vk_depth_bind_test", .apiVersion = VK_API_VERSION_1_1 };
   VkInstanceCreateInfo ici = { VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
      .pApplicationInfo = &app };
   direct_driver_chain(&ici);
   VkInstance inst;
   CHECK(create(&ici, NULL, &inst));
   LOAD(vkEnumeratePhysicalDevices);
   LOAD(vkGetPhysicalDeviceProperties);
   LOAD(vkGetPhysicalDeviceImageFormatProperties);
   LOAD(vkGetPhysicalDeviceQueueFamilyProperties);
   LOAD(vkGetPhysicalDeviceMemoryProperties);
   LOAD(vkCreateDevice);
   LOAD(vkDestroyInstance);
   uint32_t count = 16;
   VkPhysicalDevice devices[16], pd = VK_NULL_HANDLE;
   CHECK(vkEnumeratePhysicalDevices(inst, &count, devices));
   for (uint32_t i = 0; i < count; i++) {
      VkPhysicalDeviceProperties p;
      vkGetPhysicalDeviceProperties(devices[i], &p);
      if (strstr(p.deviceName, "NVK")) {
         pd = devices[i];
         printf("DEVICE=%s\n", p.deviceName);
         break;
      }
   }
   if (!pd)
      return 1;
   uint32_t nq = 32, family;
   VkQueueFamilyProperties queues[32];
   vkGetPhysicalDeviceQueueFamilyProperties(pd, &nq, queues);
   for (family = 0; family < nq; family++)
      if (queues[family].queueFlags & VK_QUEUE_GRAPHICS_BIT)
         break;
   if (family == nq)
      return 1;
   float priority = 1;
   VkDeviceQueueCreateInfo qci = { VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = family, .queueCount = 1, .pQueuePriorities = &priority };
   VkDeviceCreateInfo dci = { VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
      .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci };
   VkDevice dev;
   CHECK(vkCreateDevice(pd, &dci, NULL, &dev));
   LOAD(vkCreateImage);
   LOAD(vkGetImageMemoryRequirements2);
   LOAD(vkAllocateMemory);
   LOAD(vkBindImageMemory);
   LOAD(vkDestroyImage);
   LOAD(vkFreeMemory);
   LOAD(vkDestroyDevice);
   VkPhysicalDeviceMemoryProperties mem;
   vkGetPhysicalDeviceMemoryProperties(pd, &mem);
   unsigned tested = 0, failed = 0, skipped = 0;
   for (unsigned f = 0; f < sizeof(formats) / sizeof(formats[0]); f++) {
      for (unsigned s = 0; s < sizeof(shapes) / sizeof(shapes[0]); s++) {
         for (unsigned samples = 1; samples <= 4; samples *= 2) {
            if (shapes[s].levels > 1 && samples > 1)
               continue;
            for (unsigned transfer = 0; transfer < 2; transfer++) {
               VkImageUsageFlags usage = VK_IMAGE_USAGE_SAMPLED_BIT |
                  VK_IMAGE_USAGE_TRANSFER_SRC_BIT |
                  (f == 5 ? VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT :
                            VK_IMAGE_USAGE_DEPTH_STENCIL_ATTACHMENT_BIT) |
                  (transfer ? VK_IMAGE_USAGE_TRANSFER_DST_BIT : 0);
               VkImageFormatProperties props;
               VkResult r = vkGetPhysicalDeviceImageFormatProperties(pd,
                  formats[f].format, VK_IMAGE_TYPE_2D, VK_IMAGE_TILING_OPTIMAL,
                  usage, 0, &props);
               if (r == VK_ERROR_FORMAT_NOT_SUPPORTED ||
                   (r == VK_SUCCESS && !(props.sampleCounts & samples))) {
                  skipped += 2;
                  continue;
               }
               CHECK(r);
               for (unsigned suballoc = 0; suballoc < 2; suballoc++) {
                  VkImageCreateInfo ci = { VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
                     .imageType = VK_IMAGE_TYPE_2D, .format = formats[f].format,
                     .extent = { shapes[s].width, shapes[s].height, 1 },
                     .mipLevels = shapes[s].levels, .arrayLayers = shapes[s].layers,
                     .samples = samples, .tiling = VK_IMAGE_TILING_OPTIMAL,
                     .usage = usage };
                  VkImage image;
                  CHECK(vkCreateImage(dev, &ci, NULL, &image));
                  VkMemoryDedicatedRequirements dedicated = {
                     VK_STRUCTURE_TYPE_MEMORY_DEDICATED_REQUIREMENTS };
                  VkMemoryRequirements2 req = { VK_STRUCTURE_TYPE_MEMORY_REQUIREMENTS_2,
                     .pNext = &dedicated };
                  VkImageMemoryRequirementsInfo2 info = {
                     VK_STRUCTURE_TYPE_IMAGE_MEMORY_REQUIREMENTS_INFO_2, .image = image };
                  vkGetImageMemoryRequirements2(dev, &info, &req);
                  if (suballoc && dedicated.requiresDedicatedAllocation) {
                     skipped++;
                     vkDestroyImage(dev, image, NULL);
                     continue;
                  }
                  unsigned mt;
                  for (mt = 0; mt < mem.memoryTypeCount; mt++)
                     if ((req.memoryRequirements.memoryTypeBits & (1u << mt)) &&
                         (mem.memoryTypes[mt].propertyFlags & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT))
                        break;
                  if (mt == mem.memoryTypeCount)
                     return 1;
                  VkDeviceSize offset = suballoc ? req.memoryRequirements.alignment * 3 : 0;
                  VkMemoryDedicatedAllocateInfo dai = {
                     VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO, .image = image };
                  VkMemoryAllocateInfo ai = { VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                     .pNext = suballoc ? NULL : &dai,
                     .allocationSize = offset + req.memoryRequirements.size,
                     .memoryTypeIndex = mt };
                  VkDeviceMemory allocation;
                  CHECK(vkAllocateMemory(dev, &ai, NULL, &allocation));
                  r = vkBindImageMemory(dev, image, allocation, offset);
                  tested++;
                  failed += r != VK_SUCCESS;
                  printf("%s %ux%u layers=%u mips=%u samples=%u transfer=%u suballoc=%u "
                         "offset=0x%llx size=0x%llx align=0x%llx bind=%d\n",
                         formats[f].name, ci.extent.width, ci.extent.height,
                         ci.arrayLayers, ci.mipLevels, samples, transfer, suballoc,
                         (unsigned long long)offset,
                         (unsigned long long)req.memoryRequirements.size,
                         (unsigned long long)req.memoryRequirements.alignment, r);
                  vkDestroyImage(dev, image, NULL);
                  vkFreeMemory(dev, allocation, NULL);
               }
            }
         }
      }
   }
   vkDestroyDevice(dev, NULL);
   vkDestroyInstance(inst, NULL);
   printf("RESULT tested=%u failed=%u skipped=%u\n", tested, failed, skipped);
   return failed != 0;
}
