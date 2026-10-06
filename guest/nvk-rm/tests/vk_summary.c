/* A small `vulkaninfo --summary` through the Vulkan loader: instance version,
 * every physical device with its driver properties, memory heaps/types,
 * queue families and device extension count. Honours VK_DIRECT_DRIVER (see
 * vk_direct_driver.h), which is how it runs in an elevated Windows session
 * where the loader ignores VK_DRIVER_FILES.
 *
 *   vk_summary [-e]     (-e: list device extensions)
 */
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "vk_direct_driver.h"

int main(int argc, char **argv)
{
   int list_ext = argc > 1 && !strcmp(argv[1], "-e");
   uint32_t iv = 0;
   vkEnumerateInstanceVersion(&iv);
   printf("Vulkan instance version: %u.%u.%u\n", VK_API_VERSION_MAJOR(iv),
          VK_API_VERSION_MINOR(iv), VK_API_VERSION_PATCH(iv));

   VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .pApplicationName = "vk_summary", .apiVersion = VK_API_VERSION_1_3 };
   VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
      .pApplicationInfo = &app };
   direct_driver_chain(&ici);
   VkInstance inst;
   VkResult r = vkCreateInstance(&ici, NULL, &inst);
   if (r != VK_SUCCESS) {
      printf("vkCreateInstance: %d\n", r);
      return 1;
   }

   uint32_t npd = 0;
   vkEnumeratePhysicalDevices(inst, &npd, NULL);
   VkPhysicalDevice *pds = calloc(npd ? npd : 1, sizeof(*pds));
   vkEnumeratePhysicalDevices(inst, &npd, pds);
   printf("%u physical device(s)\n", npd);

   for (uint32_t d = 0; d < npd; d++) {
      VkPhysicalDeviceDriverProperties drv = {
         .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRIVER_PROPERTIES };
      VkPhysicalDeviceProperties2 p2 = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2,
         .pNext = &drv };
      vkGetPhysicalDeviceProperties2(pds[d], &p2);
      const VkPhysicalDeviceProperties *p = &p2.properties;
      printf("GPU%u:\n", d);
      printf("\tapiVersion         = %u.%u.%u\n", VK_API_VERSION_MAJOR(p->apiVersion),
             VK_API_VERSION_MINOR(p->apiVersion), VK_API_VERSION_PATCH(p->apiVersion));
      printf("\tdriverVersion      = 0x%x\n", p->driverVersion);
      printf("\tvendorID           = 0x%04x\n", p->vendorID);
      printf("\tdeviceID           = 0x%04x\n", p->deviceID);
      printf("\tdeviceType         = %d\n", p->deviceType);
      printf("\tdeviceName         = %s\n", p->deviceName);
      printf("\tdriverID           = %d\n", drv.driverID);
      printf("\tdriverName         = %s\n", drv.driverName);
      printf("\tdriverInfo         = %s\n", drv.driverInfo);
      printf("\tconformanceVersion = %u.%u.%u.%u\n", drv.conformanceVersion.major,
             drv.conformanceVersion.minor, drv.conformanceVersion.subminor,
             drv.conformanceVersion.patch);

      VkPhysicalDeviceMemoryProperties mp;
      vkGetPhysicalDeviceMemoryProperties(pds[d], &mp);
      for (uint32_t i = 0; i < mp.memoryHeapCount; i++)
         printf("\theap %u: %llu MiB flags 0x%x\n", i,
                (unsigned long long)(mp.memoryHeaps[i].size >> 20), mp.memoryHeaps[i].flags);
      for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
         printf("\tmemtype %u: heap %u flags 0x%x\n", i, mp.memoryTypes[i].heapIndex,
                mp.memoryTypes[i].propertyFlags);

      uint32_t nqf = 0;
      vkGetPhysicalDeviceQueueFamilyProperties(pds[d], &nqf, NULL);
      VkQueueFamilyProperties *qf = calloc(nqf ? nqf : 1, sizeof(*qf));
      vkGetPhysicalDeviceQueueFamilyProperties(pds[d], &nqf, qf);
      for (uint32_t i = 0; i < nqf; i++)
         printf("\tqueue family %u: flags 0x%x count %u\n", i, qf[i].queueFlags,
                qf[i].queueCount);
      free(qf);

      uint32_t next = 0;
      vkEnumerateDeviceExtensionProperties(pds[d], NULL, &next, NULL);
      printf("\tdevice extensions  = %u\n", next);
      if (list_ext) {
         VkExtensionProperties *e = calloc(next ? next : 1, sizeof(*e));
         vkEnumerateDeviceExtensionProperties(pds[d], NULL, &next, e);
         for (uint32_t i = 0; i < next; i++)
            printf("\t\t%s\n", e[i].extensionName);
         free(e);
      }
   }

   free(pds);
   vkDestroyInstance(inst, NULL);
   return npd ? 0 : 1;
}
