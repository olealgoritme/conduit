/*
 * vk_loader_list: what an ordinary Vulkan app sees through the system loader
 * (vulkan-1.dll from System32, no VK_DRIVER_FILES): every physical device
 * with its driver, LUID and type, then a device and an empty submit on the
 * device a typical app would pick (the first discrete GPU).
 *
 *   vk_loader_list.exe
 *
 * Exit code 0 when at least one device was found and the submit completed.
 */
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#define VK_NO_PROTOTYPES
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <string.h>

#define GIPA(inst, name) PFN_##name name = (PFN_##name)gipa(inst, #name)

int
main(void)
{
   setvbuf(stdout, NULL, _IONBF, 0);
   char sys[MAX_PATH];
   GetSystemDirectoryA(sys, sizeof(sys));
   strcat(sys, "\\vulkan-1.dll");
   HMODULE vk = LoadLibraryA(sys);
   if (!vk) {
      printf("no %s\n", sys);
      return 2;
   }
   PFN_vkGetInstanceProcAddr gipa =
      (PFN_vkGetInstanceProcAddr)(void *)GetProcAddress(vk, "vkGetInstanceProcAddr");
   GIPA(NULL, vkCreateInstance);
   VkApplicationInfo app = {VK_STRUCTURE_TYPE_APPLICATION_INFO};
   app.pApplicationName = "vk_loader_list";
   app.apiVersion = VK_API_VERSION_1_3;
   VkInstanceCreateInfo ici = {VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO};
   ici.pApplicationInfo = &app;
   VkInstance inst;
   VkResult res = vkCreateInstance(&ici, NULL, &inst);
   if (res != VK_SUCCESS) {
      printf("vkCreateInstance: %d\n", res);
      return 2;
   }
   GIPA(inst, vkEnumeratePhysicalDevices);
   GIPA(inst, vkGetPhysicalDeviceProperties2);
   GIPA(inst, vkGetPhysicalDeviceQueueFamilyProperties);
   GIPA(inst, vkCreateDevice);
   GIPA(inst, vkGetDeviceProcAddr);
   GIPA(inst, vkDestroyInstance);

   uint32_t n = 16;
   VkPhysicalDevice pd[16];
   res = vkEnumeratePhysicalDevices(inst, &n, pd);
   if (res < 0)
      n = 0;
   printf("physical devices: %u (%d)\n", n, res);
   int pick = -1;
   for (uint32_t i = 0; i < n; i++) {
      VkPhysicalDeviceDriverProperties drv = {VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRIVER_PROPERTIES};
      VkPhysicalDeviceIDProperties id = {VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ID_PROPERTIES, &drv};
      VkPhysicalDeviceProperties2 p = {VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2, &id};
      vkGetPhysicalDeviceProperties2(pd[i], &p);
      uint32_t luid[2];
      memcpy(luid, id.deviceLUID, 8);
      printf("  [%u] %s | driver %s (%s) | api %u.%u.%u | type %d | LUID %s%08x:%08x\n", i,
             p.properties.deviceName, drv.driverName, drv.driverInfo,
             VK_API_VERSION_MAJOR(p.properties.apiVersion),
             VK_API_VERSION_MINOR(p.properties.apiVersion),
             VK_API_VERSION_PATCH(p.properties.apiVersion), p.properties.deviceType,
             id.deviceLUIDValid ? "" : "(invalid) ", luid[1], luid[0]);
      if (pick < 0 && p.properties.deviceType == VK_PHYSICAL_DEVICE_TYPE_DISCRETE_GPU)
         pick = (int)i;
   }
   if (pick < 0 && n)
      pick = 0;
   if (pick < 0) {
      printf("RESULT FAIL (no device)\n");
      return 1;
   }

   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = {VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO};
   qci.queueFamilyIndex = 0;
   qci.queueCount = 1;
   qci.pQueuePriorities = &prio;
   VkDeviceCreateInfo dci = {VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO};
   dci.queueCreateInfoCount = 1;
   dci.pQueueCreateInfos = &qci;
   VkDevice dev;
   res = vkCreateDevice(pd[pick], &dci, NULL, &dev);
   if (res != VK_SUCCESS) {
      printf("vkCreateDevice on [%d]: %d\nRESULT FAIL\n", pick, res);
      return 1;
   }
#define GDPA(name) PFN_##name name = (PFN_##name)vkGetDeviceProcAddr(dev, #name)
   GDPA(vkGetDeviceQueue);
   GDPA(vkCreateFence);
   GDPA(vkQueueSubmit);
   GDPA(vkWaitForFences);
   GDPA(vkDestroyFence);
   GDPA(vkDestroyDevice);
   VkQueue q;
   vkGetDeviceQueue(dev, 0, 0, &q);
   VkFenceCreateInfo fci = {VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
   VkFence f;
   vkCreateFence(dev, &fci, NULL, &f);
   res = vkQueueSubmit(q, 0, NULL, f);
   VkResult w = vkWaitForFences(dev, 1, &f, VK_TRUE, 5000000000ull);
   printf("picked [%d]: device created, empty submit %d, fence wait %d\n", pick, res, w);
   vkDestroyFence(dev, f, NULL);
   vkDestroyDevice(dev, NULL);
   vkDestroyInstance(inst, NULL);
   int ok = res == VK_SUCCESS && w == VK_SUCCESS;
   printf("RESULT %s\n", ok ? "PASS" : "FAIL");
   return ok ? 0 : 1;
}
