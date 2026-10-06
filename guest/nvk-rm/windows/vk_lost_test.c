/*
 * Device loss on NVK-on-RM (patch 0044), without stopping the KMD: the test
 * moves the process's loss epoch in the shared KMD-view loss table
 * (guest/windows/umd_common/bridge/helios_kmdmap.h) the way librmclient does
 * when an escape reports the device removed, then checks that the device
 * answers VK_ERROR_DEVICE_LOST instead of touching its mappings, waits do not
 * hang, and teardown completes.
 *
 *   set NVK_RM=1 & set HELIOS_ICD=nvk & set VK_DIRECT_DRIVER=...\vulkan_nouveau.dll
 *   vk_lost_test.exe
 *
 * Exit 0 = PASS. A 60 s watchdog turns a hang into a failure.
 */
#include <stdio.h>
#include <stdlib.h>
#include <windows.h>

#include "../tests/vk_direct_driver.h"

#define HELIOS_KMDMAP_LOG(...) (fprintf(stderr, "  kmdmap: " __VA_ARGS__), fputc('\n', stderr))
#include "../../windows/umd_common/bridge/helios_kmdmap.h"

static int failures;
#define CHECK(cond)                                                            \
   do {                                                                        \
      if (!(cond)) {                                                           \
         fprintf(stderr, "FAIL line %d: %s\n", __LINE__, #cond);               \
         failures++;                                                           \
      }                                                                        \
   } while (0)

static DWORD WINAPI
watchdog(void *arg)
{
   (void)arg;
   Sleep(60000);
   fprintf(stderr, "FAIL: hang (60 s)\n");
   fflush(stderr);
   ExitProcess(3);
   return 0;
}

static VkResult
submit_and_wait(VkDevice dev, VkQueue q, VkFence fence, uint64_t timeout_ns)
{
   VkResult r = vkResetFences(dev, 1, &fence);
   if (r != VK_SUCCESS)
      return r;
   const VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO };
   r = vkQueueSubmit(q, 1, &si, fence);
   if (r != VK_SUCCESS)
      return r;
   return vkWaitForFences(dev, 1, &fence, VK_TRUE, timeout_ns);
}

int
main(void)
{
   CloseHandle(CreateThread(NULL, 0, watchdog, NULL, 0, NULL));

   VkApplicationInfo app = {
      .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .apiVersion = VK_API_VERSION_1_3,
   };
   VkInstanceCreateInfo ici = {
      .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
      .pApplicationInfo = &app,
   };
   direct_driver_chain(&ici);
   VkInstance inst;
   if (vkCreateInstance(&ici, NULL, &inst) != VK_SUCCESS) {
      fprintf(stderr, "FAIL: vkCreateInstance\n");
      return 1;
   }
   uint32_t n = 1;
   VkPhysicalDevice pd;
   if (vkEnumeratePhysicalDevices(inst, &n, &pd) < 0 || n == 0) {
      fprintf(stderr, "FAIL: no physical device (NVK_RM=1, HELIOS_ICD=nvk?)\n");
      return 1;
   }
   const float prio = 1.0f;
   const VkDeviceQueueCreateInfo qci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = 0,
      .queueCount = 1,
      .pQueuePriorities = &prio,
   };
   const VkDeviceCreateInfo dci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
      .queueCreateInfoCount = 1,
      .pQueueCreateInfos = &qci,
   };
   VkDevice dev;
   if (vkCreateDevice(pd, &dci, NULL, &dev) != VK_SUCCESS) {
      fprintf(stderr, "FAIL: vkCreateDevice\n");
      return 1;
   }
   VkQueue q;
   vkGetDeviceQueue(dev, 0, 0, &q);
   const VkFenceCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
   VkFence fence;
   CHECK(vkCreateFence(dev, &fci, NULL, &fence) == VK_SUCCESS);
   const VkSemaphoreTypeCreateInfo stci = {
      .sType = VK_STRUCTURE_TYPE_SEMAPHORE_TYPE_CREATE_INFO,
      .semaphoreType = VK_SEMAPHORE_TYPE_TIMELINE,
   };
   const VkSemaphoreCreateInfo sci = {
      .sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO,
      .pNext = &stci,
   };
   VkSemaphore timeline;
   CHECK(vkCreateSemaphore(dev, &sci, NULL, &timeline) == VK_SUCCESS);

   /* 1. A healthy device. */
   VkResult r = submit_and_wait(dev, q, fence, 5000000000ull);
   printf("before loss: submit+wait -> %d\n", r);
   CHECK(r == VK_SUCCESS);

   /* 2. The KMD "goes away": move the loss epoch, as librmclient does on a
    *    device-removed escape status. */
   const int32_t epoch = helios_kmdmap_attach();
   CHECK(helios_kmdmap_t != NULL);
   helios_kmdmap_mark_lost(epoch);
   printf("loss marked (epoch %d -> %ld)\n", epoch,
          helios_kmdmap_t ? (long)helios_kmdmap_t->epoch : -1L);

   /* 3. Every path that would touch a KMD view answers DEVICE_LOST, fast. */
   const DWORD t0 = GetTickCount();
   r = submit_and_wait(dev, q, fence, 5000000000ull);
   printf("after loss: submit+wait -> %d\n", r);
   CHECK(r == VK_ERROR_DEVICE_LOST);

   uint64_t value = 0;
   r = vkGetSemaphoreCounterValue(dev, timeline, &value);
   printf("after loss: vkGetSemaphoreCounterValue -> %d\n", r);
   CHECK(r == VK_ERROR_DEVICE_LOST || r == VK_SUCCESS);

   const VkSemaphoreSignalInfo ssi = {
      .sType = VK_STRUCTURE_TYPE_SEMAPHORE_SIGNAL_INFO,
      .semaphore = timeline,
      .value = 1,
   };
   r = vkSignalSemaphore(dev, &ssi);
   printf("after loss: vkSignalSemaphore -> %d\n", r);
   CHECK(r == VK_ERROR_DEVICE_LOST || r == VK_SUCCESS);

   const uint64_t wait_value = 1000;
   const VkSemaphoreWaitInfo swi = {
      .sType = VK_STRUCTURE_TYPE_SEMAPHORE_WAIT_INFO,
      .semaphoreCount = 1,
      .pSemaphores = &timeline,
      .pValues = &wait_value,
   };
   r = vkWaitSemaphores(dev, &swi, 10000000000ull);
   printf("after loss: vkWaitSemaphores(never signaled) -> %d\n", r);
   CHECK(r == VK_ERROR_DEVICE_LOST);

   r = vkDeviceWaitIdle(dev);
   printf("after loss: vkDeviceWaitIdle -> %d\n", r);
   CHECK(r == VK_ERROR_DEVICE_LOST || r == VK_SUCCESS);

   const DWORD dt = GetTickCount() - t0;
   printf("after-loss calls took %lu ms\n", (unsigned long)dt);
   CHECK(dt < 5000); /* no wait ran into its timeout */

   /* 4. Teardown completes. */
   vkDestroySemaphore(dev, timeline, NULL);
   vkDestroyFence(dev, fence, NULL);
   vkDestroyDevice(dev, NULL);
   vkDestroyInstance(inst, NULL);
   helios_kmdmap_detach();

   printf("%s (%d failures)\n", failures ? "FAILED" : "PASSED", failures);
   return failures ? 1 : 0;
}
