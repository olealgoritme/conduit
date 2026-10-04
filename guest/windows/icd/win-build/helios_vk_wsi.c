/*
 * helios_vk_wsi.c — Phase 6 WSI smoke test for the Helios venus ICD.
 *
 * Exercises the full Win32 swapchain present path headlessly: create a window,
 * VK_KHR_win32_surface, query presentation support + surface caps/formats,
 * create a device + VK_KHR_swapchain, then for a few frames acquire an image,
 * clear it to an animated color, submit, and present. The present uses Mesa
 * wsi_common_win32's software (GDI/DIB) blit since venus selects the sw WSI
 * device on Windows (no dma-buf). Bounded loops + acquire timeout so it can run
 * over the session-0 SSH without hanging (the window won't be visible there;
 * for a visible cube use vkcube on the interactive spice desktop).
 *
 * Build (mingw, on win11):
 *   gcc -O2 -o C:\Users\Rupansh\helios_vk_wsi.exe Z:\icd\win-build\helios_vk_wsi.c \
 *       -IZ:\icd\mesa\include -lgdi32 -luser32
 */
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>

#define VK_USE_PLATFORM_WIN32_KHR
#define VK_NO_PROTOTYPES
#include <vulkan/vulkan.h>

#define ILOAD(name) (PFN_##name)(void *) gipa(inst, #name)
#define DLOAD(name) (PFN_##name)(void *) gdpa(dev, #name)

#define CHECK(expr)                                                            \
   do {                                                                        \
      VkResult _r = (expr);                                                    \
      fprintf(stderr, "  %-46s => %d %s\n", #expr, _r,                         \
              _r >= 0 ? "OK" : "FAIL");                                        \
      fflush(stderr);                                                          \
      if (_r < 0)                                                              \
         return 10;                                                            \
   } while (0)

static LRESULT CALLBACK
wndproc(HWND h, UINT m, WPARAM w, LPARAM l)
{
   return DefWindowProcW(h, m, w, l);
}

int
main(void)
{
   HMODULE vk = LoadLibraryW(L"vulkan-1.dll");
   if (!vk)
      return 1;
   PFN_vkGetInstanceProcAddr gipa =
      (PFN_vkGetInstanceProcAddr)(void *)GetProcAddress(vk, "vkGetInstanceProcAddr");
   PFN_vkCreateInstance pCreateInstance = (PFN_vkCreateInstance)gipa(NULL, "vkCreateInstance");

   const char *iexts[] = { "VK_KHR_surface", "VK_KHR_win32_surface" };
   const VkApplicationInfo app = {
      .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .pApplicationName = "helios_vk_wsi",
      .apiVersion = VK_API_VERSION_1_1,
   };
   const VkInstanceCreateInfo ici = {
      .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
      .pApplicationInfo = &app,
      .enabledExtensionCount = 2,
      .ppEnabledExtensionNames = iexts,
   };
   VkInstance inst = VK_NULL_HANDLE;
   CHECK(pCreateInstance(&ici, NULL, &inst));

   PFN_vkEnumeratePhysicalDevices pEnum = ILOAD(vkEnumeratePhysicalDevices);
   PFN_vkGetPhysicalDeviceProperties pProps = ILOAD(vkGetPhysicalDeviceProperties);
   PFN_vkGetPhysicalDeviceQueueFamilyProperties pQFP =
      ILOAD(vkGetPhysicalDeviceQueueFamilyProperties);
   PFN_vkCreateDevice pCreateDevice = ILOAD(vkCreateDevice);
   PFN_vkGetDeviceProcAddr gdpa = ILOAD(vkGetDeviceProcAddr);
   PFN_vkCreateWin32SurfaceKHR pCreateSurf = ILOAD(vkCreateWin32SurfaceKHR);
   PFN_vkGetPhysicalDeviceWin32PresentationSupportKHR pPresSup =
      ILOAD(vkGetPhysicalDeviceWin32PresentationSupportKHR);
   PFN_vkGetPhysicalDeviceSurfaceSupportKHR pSurfSup =
      ILOAD(vkGetPhysicalDeviceSurfaceSupportKHR);
   PFN_vkGetPhysicalDeviceSurfaceCapabilitiesKHR pSurfCaps =
      ILOAD(vkGetPhysicalDeviceSurfaceCapabilitiesKHR);
   PFN_vkGetPhysicalDeviceSurfaceFormatsKHR pSurfFmts =
      ILOAD(vkGetPhysicalDeviceSurfaceFormatsKHR);

   /* Create a window (hidden/unviewable in session 0; that's fine for the API). */
   HINSTANCE hinst = GetModuleHandleW(NULL);
   WNDCLASSW wc = { .lpfnWndProc = wndproc, .hInstance = hinst,
                    .lpszClassName = L"HeliosWsiTest" };
   RegisterClassW(&wc);
   HWND hwnd = CreateWindowExW(0, L"HeliosWsiTest", L"helios_vk_wsi",
                               WS_OVERLAPPEDWINDOW, CW_USEDEFAULT, CW_USEDEFAULT,
                               640, 480, NULL, NULL, hinst, NULL);
   if (!hwnd) {
      fprintf(stderr, "CreateWindowEx failed: %lu\n", GetLastError());
      return 2;
   }
   ShowWindow(hwnd, SW_SHOW);

   VkSurfaceKHR surf = VK_NULL_HANDLE;
   const VkWin32SurfaceCreateInfoKHR sci = {
      .sType = VK_STRUCTURE_TYPE_WIN32_SURFACE_CREATE_INFO_KHR,
      .hinstance = hinst,
      .hwnd = hwnd,
   };
   CHECK(pCreateSurf(inst, &sci, NULL, &surf));

   uint32_t count = 0;
   CHECK(pEnum(inst, &count, NULL));
   VkPhysicalDevice devs[8];
   if (count > 8)
      count = 8;
   CHECK(pEnum(inst, &count, devs));
   /* Pick the first device that supports present on this surface; prefer a
    * "Venus" device (skip llvmpipe if a real one presents). */
   VkPhysicalDevice phys = VK_NULL_HANDLE;
   uint32_t qfi = 0;
   for (uint32_t d = 0; d < count && phys == VK_NULL_HANDLE; d++) {
      VkPhysicalDeviceProperties pp;
      pProps(devs[d], &pp);
      uint32_t qn = 0;
      pQFP(devs[d], &qn, NULL);
      VkQueueFamilyProperties qfs[16];
      if (qn > 16)
         qn = 16;
      pQFP(devs[d], &qn, qfs);
      for (uint32_t q = 0; q < qn; q++) {
         VkBool32 sup = 0;
         pSurfSup(devs[d], q, surf, &sup);
         const VkBool32 win32sup = pPresSup(devs[d], q);
         if ((qfs[q].queueFlags & VK_QUEUE_GRAPHICS_BIT) && sup && win32sup) {
            phys = devs[d];
            qfi = q;
            fprintf(stderr, "Using device \"%s\" queue family %u "
                            "(surface+win32 present supported)\n", pp.deviceName, q);
            break;
         }
      }
   }
   if (phys == VK_NULL_HANDLE) {
      fprintf(stderr, "FAIL: no device/queue supports present on the surface\n");
      return 3;
   }

   VkSurfaceCapabilitiesKHR caps;
   CHECK(pSurfCaps(phys, surf, &caps));
   uint32_t fmtcount = 0;
   CHECK(pSurfFmts(phys, surf, &fmtcount, NULL));
   if (fmtcount == 0) {
      fprintf(stderr, "FAIL: no surface formats\n");
      return 3;
   }
   VkSurfaceFormatKHR fmts[64];
   if (fmtcount > 64)
      fmtcount = 64;
   CHECK(pSurfFmts(phys, surf, &fmtcount, fmts));
   VkSurfaceFormatKHR fmt = fmts[0];
   fprintf(stderr, "  surface format=%d colorspace=%d  extent=%ux%u  minImages=%u\n",
           fmt.format, fmt.colorSpace, caps.currentExtent.width,
           caps.currentExtent.height, caps.minImageCount);

   const float prio = 1.0f;
   const char *dexts[] = { "VK_KHR_swapchain" };
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
      .enabledExtensionCount = 1,
      .ppEnabledExtensionNames = dexts,
   };
   VkDevice dev = VK_NULL_HANDLE;
   CHECK(pCreateDevice(phys, &dci, NULL, &dev));

   PFN_vkGetDeviceQueue pGetQueue = DLOAD(vkGetDeviceQueue);
   PFN_vkCreateSwapchainKHR pCreateSwap = DLOAD(vkCreateSwapchainKHR);
   PFN_vkGetSwapchainImagesKHR pGetImgs = DLOAD(vkGetSwapchainImagesKHR);
   PFN_vkAcquireNextImageKHR pAcquire = DLOAD(vkAcquireNextImageKHR);
   PFN_vkQueuePresentKHR pPresent = DLOAD(vkQueuePresentKHR);
   PFN_vkCreateSemaphore pCreateSem = DLOAD(vkCreateSemaphore);
   PFN_vkCreateCommandPool pCreatePool = DLOAD(vkCreateCommandPool);
   PFN_vkAllocateCommandBuffers pAllocCmd = DLOAD(vkAllocateCommandBuffers);
   PFN_vkBeginCommandBuffer pBegin = DLOAD(vkBeginCommandBuffer);
   PFN_vkEndCommandBuffer pEnd = DLOAD(vkEndCommandBuffer);
   PFN_vkCmdPipelineBarrier pBarrier = DLOAD(vkCmdPipelineBarrier);
   PFN_vkCmdClearColorImage pClear = DLOAD(vkCmdClearColorImage);
   PFN_vkQueueSubmit pSubmit = DLOAD(vkQueueSubmit);
   PFN_vkQueueWaitIdle pQWaitIdle = DLOAD(vkQueueWaitIdle);
   PFN_vkResetCommandBuffer pResetCmd = DLOAD(vkResetCommandBuffer);

   VkQueue queue = VK_NULL_HANDLE;
   pGetQueue(dev, qfi, 0, &queue);

   VkExtent2D extent = caps.currentExtent;
   if (extent.width == 0xFFFFFFFFu) {
      extent.width = 640;
      extent.height = 480;
   }
   uint32_t want = caps.minImageCount + 1;
   if (caps.maxImageCount && want > caps.maxImageCount)
      want = caps.maxImageCount;
   const VkSwapchainCreateInfoKHR scinfo = {
      .sType = VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR,
      .surface = surf,
      .minImageCount = want,
      .imageFormat = fmt.format,
      .imageColorSpace = fmt.colorSpace,
      .imageExtent = extent,
      .imageArrayLayers = 1,
      .imageUsage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT |
                    VK_IMAGE_USAGE_TRANSFER_DST_BIT,
      .imageSharingMode = VK_SHARING_MODE_EXCLUSIVE,
      .preTransform = caps.currentTransform,
      .compositeAlpha = VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR,
      .presentMode = VK_PRESENT_MODE_FIFO_KHR,
      .clipped = VK_TRUE,
   };
   VkSwapchainKHR swap = VK_NULL_HANDLE;
   CHECK(pCreateSwap(dev, &scinfo, NULL, &swap));

   uint32_t imgcount = 0;
   CHECK(pGetImgs(dev, swap, &imgcount, NULL));
   VkImage imgs[8];
   if (imgcount > 8)
      imgcount = 8;
   CHECK(pGetImgs(dev, swap, &imgcount, imgs));
   fprintf(stderr, "  swapchain created: %u images\n", imgcount);

   VkSemaphore acqSem, rndSem;
   const VkSemaphoreCreateInfo seminfo = { .sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO };
   CHECK(pCreateSem(dev, &seminfo, NULL, &acqSem));
   CHECK(pCreateSem(dev, &seminfo, NULL, &rndSem));

   VkCommandPool pool;
   const VkCommandPoolCreateInfo pci = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
      .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
      .queueFamilyIndex = qfi,
   };
   CHECK(pCreatePool(dev, &pci, NULL, &pool));
   VkCommandBuffer cmd;
   const VkCommandBufferAllocateInfo cbai = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
      .commandPool = pool,
      .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
      .commandBufferCount = 1,
   };
   CHECK(pAllocCmd(dev, &cbai, &cmd));

   const VkImageSubresourceRange range = {
      .aspectMask = VK_IMAGE_ASPECT_COLOR_BIT, .levelCount = 1, .layerCount = 1,
   };

   int FRAMES = 8;
   const char *frames_env = getenv("HELIOS_WSI_FRAMES");
   if (frames_env) {
      int parsed = atoi(frames_env);
      if (parsed > 0)
         FRAMES = parsed;
   }
   for (int f = 0; f < FRAMES; f++) {
      MSG msg;
      while (PeekMessageW(&msg, NULL, 0, 0, PM_REMOVE)) {
         TranslateMessage(&msg);
         DispatchMessageW(&msg);
      }

      fprintf(stderr, "  frame %d: acquiring...\n", f); fflush(stderr);
      uint32_t idx = 0;
      VkResult ar = pAcquire(dev, swap, 1000000000ull /* 1s */, acqSem,
                             VK_NULL_HANDLE, &idx);
      fprintf(stderr, "  frame %d: acquired idx=%u r=%d, recording...\n", f, idx, ar);
      fflush(stderr);
      if (ar < 0) {
         fprintf(stderr, "  frame %d: vkAcquireNextImageKHR => %d FAIL\n", f, ar);
         return 11;
      }

      pResetCmd(cmd, 0);
      const VkCommandBufferBeginInfo bi = {
         .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
         .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
      };
      pBegin(cmd, &bi);
      VkImageMemoryBarrier toDst = {
         .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
         .srcAccessMask = 0,
         .dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
         .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
         .newLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
         .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
         .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
         .image = imgs[idx],
         .subresourceRange = range,
      };
      pBarrier(cmd, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,
               VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL, 0, NULL, 1, &toDst);
      VkClearColorValue color = {
         .float32 = { (f % 3 == 0) ? 1.0f : 0.1f, (f % 3 == 1) ? 1.0f : 0.1f,
                      (f % 3 == 2) ? 1.0f : 0.3f, 1.0f },
      };
      pClear(cmd, imgs[idx], VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, &color, 1, &range);
      VkImageMemoryBarrier toPresent = toDst;
      toPresent.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
      toPresent.dstAccessMask = 0;
      toPresent.oldLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL;
      toPresent.newLayout = VK_IMAGE_LAYOUT_PRESENT_SRC_KHR;
      pBarrier(cmd, VK_PIPELINE_STAGE_TRANSFER_BIT,
               VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, 0, 0, NULL, 0, NULL, 1,
               &toPresent);
      pEnd(cmd);

      const VkPipelineStageFlags waitStage = VK_PIPELINE_STAGE_TRANSFER_BIT;
      const VkSubmitInfo si = {
         .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
         .waitSemaphoreCount = 1,
         .pWaitSemaphores = &acqSem,
         .pWaitDstStageMask = &waitStage,
         .commandBufferCount = 1,
         .pCommandBuffers = &cmd,
         .signalSemaphoreCount = 1,
         .pSignalSemaphores = &rndSem,
      };
      fprintf(stderr, "  frame %d: submitting...\n", f); fflush(stderr);
      VkResult sr = pSubmit(queue, 1, &si, VK_NULL_HANDLE);
      if (sr < 0) {
         fprintf(stderr, "  frame %d: vkQueueSubmit => %d FAIL\n", f, sr);
         return 12;
      }
      fprintf(stderr, "  frame %d: submitted, presenting...\n", f); fflush(stderr);

      const VkPresentInfoKHR pinfo = {
         .sType = VK_STRUCTURE_TYPE_PRESENT_INFO_KHR,
         .waitSemaphoreCount = 1,
         .pWaitSemaphores = &rndSem,
         .swapchainCount = 1,
         .pSwapchains = &swap,
         .pImageIndices = &idx,
      };
      VkResult pr = pPresent(queue, &pinfo);
      fprintf(stderr, "  frame %d: acquire idx=%u submit=0 present=%d %s\n", f,
              idx, pr, pr >= 0 ? "OK" : "FAIL");
      fflush(stderr);
      if (pr < 0)
         return 13;

      VkResult wr = pQWaitIdle(queue); /* simple per-frame sync for the smoke test */
      fprintf(stderr, "  frame %d: vkQueueWaitIdle => %d\n", f, wr);
      fflush(stderr);
   }

   fprintf(stderr, "PASS: WSI swapchain present path (%d frames)\n", FRAMES);
   return 0;
}
