/*
 * helios_vk_smoke.c — Phase 5 headless bring-up smoke test for the Helios venus
 * Vulkan ICD. Loads vulkan-1.dll (the Khronos loader) at runtime, creates a
 * Vulkan instance (which drives the loader -> vulkan_virtio.dll ->
 * vn_renderer_create_helios -> the Helios KMD IOCTLs -> host venus), then
 * enumerates physical devices and prints the driver/device name.
 *
 * Build (mingw, on win11):
 *   gcc -O2 -o helios_vk_smoke.exe Z:\icd\win-build\helios_vk_smoke.c -IZ:\icd\mesa\include
 * Run (point the loader at the just-built ICD; mingw bin on PATH for libwinpthread):
 *   $env:VK_DRIVER_FILES="C:\Users\Rupansh\helios-mesa-mingw\src\virtio\vulkan\virtio_devenv_icd.x86_64.json"
 *   $env:VK_LOADER_DEBUG="all"; $env:VN_DEBUG="init"
 *   .\helios_vk_smoke.exe
 *
 * No link-time Vulkan dependency (VK_NO_PROTOTYPES + GetProcAddress), so it needs
 * only the Vulkan headers from the Mesa tree.
 */
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <stdint.h>
#include <stdio.h>

#define VK_NO_PROTOTYPES
#include <vulkan/vulkan.h>

#define LOAD(name) (PFN_##name)(void *) GetProcAddress(vk, #name)

int
main(void)
{
   HMODULE vk = LoadLibraryW(L"vulkan-1.dll");
   if (!vk) {
      fprintf(stderr, "FAIL: LoadLibrary(vulkan-1.dll) err %lu\n",
              (unsigned long)GetLastError());
      return 1;
   }

   PFN_vkGetInstanceProcAddr gipa = LOAD(vkGetInstanceProcAddr);
   if (!gipa) {
      fprintf(stderr, "FAIL: no vkGetInstanceProcAddr\n");
      return 1;
   }

   PFN_vkCreateInstance vkCreateInstance =
      (PFN_vkCreateInstance)gipa(NULL, "vkCreateInstance");
   if (!vkCreateInstance) {
      fprintf(stderr, "FAIL: no vkCreateInstance\n");
      return 1;
   }

   const VkApplicationInfo app = {
      .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .pApplicationName = "helios_vk_smoke",
      .apiVersion = VK_API_VERSION_1_1,
   };
   const VkInstanceCreateInfo ci = {
      .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
      .pApplicationInfo = &app,
   };

   fprintf(stderr, "[1] vkCreateInstance ...\n");
   fflush(stderr);
   VkInstance inst = VK_NULL_HANDLE;
   VkResult r = vkCreateInstance(&ci, NULL, &inst);
   fprintf(stderr, "[1] vkCreateInstance => %d (%s)\n", r,
           r == VK_SUCCESS ? "VK_SUCCESS" : "FAIL");
   fflush(stderr);
   if (r != VK_SUCCESS)
      return 2;

   PFN_vkEnumeratePhysicalDevices vkEnumeratePhysicalDevices =
      (PFN_vkEnumeratePhysicalDevices)gipa(inst, "vkEnumeratePhysicalDevices");
   PFN_vkGetPhysicalDeviceProperties vkGetPhysicalDeviceProperties =
      (PFN_vkGetPhysicalDeviceProperties)gipa(inst, "vkGetPhysicalDeviceProperties");
   PFN_vkGetPhysicalDeviceProperties2 vkGetPhysicalDeviceProperties2 =
      (PFN_vkGetPhysicalDeviceProperties2)gipa(inst, "vkGetPhysicalDeviceProperties2");

   uint32_t count = 0;
   fprintf(stderr, "[2] vkEnumeratePhysicalDevices ...\n");
   fflush(stderr);
   r = vkEnumeratePhysicalDevices(inst, &count, NULL);
   fprintf(stderr, "[2] count => %u (r=%d)\n", count, r);
   fflush(stderr);
   if (r != VK_SUCCESS || count == 0)
      return 3;

   VkPhysicalDevice devs[8];
   if (count > 8)
      count = 8;
   vkEnumeratePhysicalDevices(inst, &count, devs);

   for (uint32_t i = 0; i < count; i++) {
      VkPhysicalDeviceProperties props;
      vkGetPhysicalDeviceProperties(devs[i], &props);
      fprintf(stderr, "[3] device[%u]: \"%s\" apiVersion %u.%u.%u type %d\n", i,
              props.deviceName, VK_API_VERSION_MAJOR(props.apiVersion),
              VK_API_VERSION_MINOR(props.apiVersion),
              VK_API_VERSION_PATCH(props.apiVersion), props.deviceType);

      if (vkGetPhysicalDeviceProperties2) {
         VkPhysicalDeviceDriverProperties drv = {
            .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRIVER_PROPERTIES,
         };
         VkPhysicalDeviceProperties2 p2 = {
            .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2,
            .pNext = &drv,
         };
         vkGetPhysicalDeviceProperties2(devs[i], &p2);
         fprintf(stderr, "[3] device[%u]: driverName \"%s\" driverInfo \"%s\"\n", i,
                 drv.driverName, drv.driverInfo);
      }
      fflush(stderr);
   }

   fprintf(stderr, "PASS: instance + %u physical device(s)\n", count);
   return 0;
}
