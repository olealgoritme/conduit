/* Optional VK_LUNARG_direct_driver_loading for the NVK-on-RM tests.
 *
 * The Windows Vulkan loader ignores VK_DRIVER_FILES / VK_ICD_FILENAMES in an
 * elevated process (an administrator's ssh session is one), so the tests can
 * hand it the ICD directly: with VK_DIRECT_DRIVER=path\to\vulkan_nouveau.dll
 * the DLL is loaded and passed as the only driver
 * (VK_DIRECT_DRIVER_LOADING_MODE_EXCLUSIVE_LUNARG). Unset: normal loader
 * discovery. Linux: always normal discovery.
 *
 *   VkInstanceCreateInfo ici = { ... };
 *   direct_driver_chain(&ici);   // before vkCreateInstance
 */
#ifndef VK_DIRECT_DRIVER_H
#define VK_DIRECT_DRIVER_H

#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>

#ifdef _WIN32
#include <windows.h>

static VkDirectDriverLoadingInfoLUNARG direct_driver_info;
static VkDirectDriverLoadingListLUNARG direct_driver_list;
static const char *direct_driver_exts[64];

static void direct_driver_chain(VkInstanceCreateInfo *ici)
{
   const char *path = getenv("VK_DIRECT_DRIVER");
   if (!path || !*path)
      return;
   HMODULE dll = LoadLibraryA(path);
   if (!dll) {
      fprintf(stderr, "VK_DIRECT_DRIVER: LoadLibrary(%s) failed: %lu\n", path,
              (unsigned long)GetLastError());
      exit(1);
   }
   PFN_vkGetInstanceProcAddrLUNARG gipa = (PFN_vkGetInstanceProcAddrLUNARG)(void *)
      GetProcAddress(dll, "vk_icdGetInstanceProcAddr");
   if (!gipa) {
      fprintf(stderr, "VK_DIRECT_DRIVER: %s has no vk_icdGetInstanceProcAddr\n", path);
      exit(1);
   }
   direct_driver_info = (VkDirectDriverLoadingInfoLUNARG) {
      .sType = VK_STRUCTURE_TYPE_DIRECT_DRIVER_LOADING_INFO_LUNARG,
      .pfnGetInstanceProcAddr = gipa,
   };
   direct_driver_list = (VkDirectDriverLoadingListLUNARG) {
      .sType = VK_STRUCTURE_TYPE_DIRECT_DRIVER_LOADING_LIST_LUNARG,
      .pNext = ici->pNext,
      .mode = VK_DIRECT_DRIVER_LOADING_MODE_EXCLUSIVE_LUNARG,
      .driverCount = 1,
      .pDrivers = &direct_driver_info,
   };
   ici->pNext = &direct_driver_list;

   uint32_t n = ici->enabledExtensionCount;
   if (n + 1 > sizeof(direct_driver_exts) / sizeof(direct_driver_exts[0]))
      exit(1);
   for (uint32_t i = 0; i < n; i++)
      direct_driver_exts[i] = ici->ppEnabledExtensionNames[i];
   direct_driver_exts[n] = VK_LUNARG_DIRECT_DRIVER_LOADING_EXTENSION_NAME;
   ici->enabledExtensionCount = n + 1;
   ici->ppEnabledExtensionNames = direct_driver_exts;
   fprintf(stderr, "VK_DIRECT_DRIVER: %s (exclusive)\n", path);
}
#else
static inline void direct_driver_chain(VkInstanceCreateInfo *ici) { (void)ici; }
#endif

#endif
