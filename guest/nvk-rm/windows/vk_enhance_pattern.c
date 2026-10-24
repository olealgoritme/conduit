/* A Win32 Vulkan test pattern for the composed present's upscaling and
 * frame generation (patches-windows-dxvk 0007, guest/nvk-rm/docs/enhance.md).
 *
 *   vk_enhance_pattern.exe [seconds] [fps cap] [render WxH] [window WxH|full]
 *                          [immediate|fifo|mailbox] [box speed]
 *
 * Draws, by clears only (no shaders): a fine grid, a white box moving at a
 * constant speed in pixels per second (time-based, so a generated picture
 * shows it between two frames), a static "HUD" box and a frame counter in
 * binary.  The box speed is in widths per second (default 0.25).  "full" (default) is a borderless window covering the primary
 * monitor.  The swapchain is the render size when the surface leaves the
 * size to the swapchain (currentExtent 0xFFFFFFFF: an upscaler is on), else
 * the window's.  A fps cap (0: none) sleeps between frames like an
 * fps-locked game.  Prints the frames rendered, the frame rate and the time
 * vkQueuePresentKHR took (average and worst): what frame generation must not
 * add to.  With VK_DIRECT_DRIVER (tests/vk_direct_driver.h) the given NVK.
 */
#define VK_USE_PLATFORM_WIN32_KHR
#include <vulkan/vulkan.h>
#include <windows.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "../tests/vk_direct_driver.h"

#define CHECK(x) do { VkResult r_ = (x); if (r_ < 0) { \
   fprintf(stderr, "%s:%d: %s = %d\n", __FILE__, __LINE__, #x, r_); exit(1); } } while (0)

static LRESULT CALLBACK wndproc(HWND h, UINT m, WPARAM w, LPARAM l)
{
   if (m == WM_CLOSE) { PostQuitMessage(0); return 0; }
   return DefWindowProcA(h, m, w, l);
}

static double now_s(void)
{
   static LARGE_INTEGER f;
   LARGE_INTEGER t;
   if (!f.QuadPart)
      QueryPerformanceFrequency(&f);
   QueryPerformanceCounter(&t);
   return (double)t.QuadPart / (double)f.QuadPart;
}

static void parse_wh(const char *s, uint32_t *w, uint32_t *h)
{
   unsigned a, b;
   if (s && sscanf(s, "%ux%u", &a, &b) == 2) { *w = a; *h = b; }
}

int main(int argc, char **argv)
{
   double seconds = argc > 1 ? atof(argv[1]) : 10.0;
   double cap = argc > 2 ? atof(argv[2]) : 0.0;
   uint32_t rw = 1280, rh = 720;
   parse_wh(argc > 3 ? argv[3] : NULL, &rw, &rh);
   int full = 1;
   uint32_t ww = 1280, wh = 720;
   if (argc > 4 && strcmp(argv[4], "full") != 0) {
      full = 0;
      parse_wh(argv[4], &ww, &wh);
   }
   VkPresentModeKHR mode = VK_PRESENT_MODE_IMMEDIATE_KHR;
   if (argc > 5 && !strcmp(argv[5], "fifo")) mode = VK_PRESENT_MODE_FIFO_KHR;
   if (argc > 5 && !strcmp(argv[5], "mailbox")) mode = VK_PRESENT_MODE_MAILBOX_KHR;
   const double speed = argc > 6 ? atof(argv[6]) : 0.25;

   timeBeginPeriod(1);
   SetProcessDPIAware();
   HINSTANCE inst = GetModuleHandleA(NULL);
   WNDCLASSA wc = { .lpfnWndProc = wndproc, .hInstance = inst,
                    .lpszClassName = "vkpattern", .hCursor = LoadCursor(NULL, IDC_ARROW) };
   RegisterClassA(&wc);
   HWND hwnd;
   if (full) {
      ww = (uint32_t)GetSystemMetrics(SM_CXSCREEN);
      wh = (uint32_t)GetSystemMetrics(SM_CYSCREEN);
      hwnd = CreateWindowExA(0, "vkpattern", "vk_enhance_pattern", WS_POPUP | WS_VISIBLE,
                             0, 0, (int)ww, (int)wh, NULL, NULL, inst, NULL);
   } else {
      RECT r = { 0, 0, (LONG)ww, (LONG)wh };
      AdjustWindowRect(&r, WS_OVERLAPPEDWINDOW, FALSE);
      hwnd = CreateWindowExA(0, "vkpattern", "vk_enhance_pattern",
                             WS_OVERLAPPEDWINDOW | WS_VISIBLE, 40, 40,
                             r.right - r.left, r.bottom - r.top, NULL, NULL, inst, NULL);
   }
   SetForegroundWindow(hwnd);

   const char *iext[] = { VK_KHR_SURFACE_EXTENSION_NAME, VK_KHR_WIN32_SURFACE_EXTENSION_NAME };
   VkApplicationInfo ai = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
                            .apiVersion = VK_API_VERSION_1_3 };
   VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                                .pApplicationInfo = &ai, .enabledExtensionCount = 2,
                                .ppEnabledExtensionNames = iext };
   direct_driver_chain(&ici);
   VkInstance instance;
   CHECK(vkCreateInstance(&ici, NULL, &instance));
   uint32_t npd = 8;
   VkPhysicalDevice pds[8];
   CHECK(vkEnumeratePhysicalDevices(instance, &npd, pds));
   VkPhysicalDevice pd = pds[0];
   VkPhysicalDeviceProperties props;
   vkGetPhysicalDeviceProperties(pd, &props);

   VkWin32SurfaceCreateInfoKHR sci = { .sType = VK_STRUCTURE_TYPE_WIN32_SURFACE_CREATE_INFO_KHR,
                                       .hinstance = inst, .hwnd = hwnd };
   VkSurfaceKHR surface;
   CHECK(vkCreateWin32SurfaceKHR(instance, &sci, NULL, &surface));

   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                                   .queueFamilyIndex = 0, .queueCount = 1,
                                   .pQueuePriorities = &prio };
   const char *dext[] = { VK_KHR_SWAPCHAIN_EXTENSION_NAME };
   VkPhysicalDeviceVulkan13Features f13 = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES,
                                            .dynamicRendering = VK_TRUE };
   VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .pNext = &f13,
                              .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci,
                              .enabledExtensionCount = 1, .ppEnabledExtensionNames = dext };
   VkDevice dev;
   CHECK(vkCreateDevice(pd, &dci, NULL, &dev));
   VkQueue queue;
   vkGetDeviceQueue(dev, 0, 0, &queue);

   VkSurfaceCapabilitiesKHR caps;
   CHECK(vkGetPhysicalDeviceSurfaceCapabilitiesKHR(pd, surface, &caps));
   VkExtent2D ext = caps.currentExtent;
   int free_size = ext.width == UINT32_MAX;
   if (free_size)
      ext = (VkExtent2D){ rw, rh };
   printf("device %s; window %ux%u; surface currentExtent %s; swapchain %ux%u\n",
          props.deviceName, ww, wh, free_size ? "free (an upscaler is on)" : "fixed",
          ext.width, ext.height);

   uint32_t nimg = caps.minImageCount < 3 ? 3 : caps.minImageCount;
   if (caps.maxImageCount && nimg > caps.maxImageCount)
      nimg = caps.maxImageCount;
   VkSwapchainCreateInfoKHR swci = {
      .sType = VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR, .surface = surface,
      .minImageCount = nimg, .imageFormat = VK_FORMAT_B8G8R8A8_UNORM,
      .imageColorSpace = VK_COLOR_SPACE_SRGB_NONLINEAR_KHR, .imageExtent = ext,
      .imageArrayLayers = 1,
      .imageUsage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT,
      .imageSharingMode = VK_SHARING_MODE_EXCLUSIVE,
      .preTransform = VK_SURFACE_TRANSFORM_IDENTITY_BIT_KHR,
      .compositeAlpha = VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR, .presentMode = mode, .clipped = VK_TRUE,
   };
   VkSwapchainKHR sc;
   CHECK(vkCreateSwapchainKHR(dev, &swci, NULL, &sc));
   uint32_t n = 0;
   CHECK(vkGetSwapchainImagesKHR(dev, sc, &n, NULL));
   VkImage *imgs = calloc(n, sizeof(*imgs));
   VkImageView *views = calloc(n, sizeof(*views));
   CHECK(vkGetSwapchainImagesKHR(dev, sc, &n, imgs));
   for (uint32_t i = 0; i < n; i++) {
      VkImageViewCreateInfo v = { .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
                                  .image = imgs[i], .viewType = VK_IMAGE_VIEW_TYPE_2D,
                                  .format = VK_FORMAT_B8G8R8A8_UNORM,
                                  .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
      CHECK(vkCreateImageView(dev, &v, NULL, &views[i]));
   }

   VkCommandPoolCreateInfo pci = { .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
                                   .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT };
   VkCommandPool pool;
   CHECK(vkCreateCommandPool(dev, &pci, NULL, &pool));
#define FRAMES 2
   VkCommandBuffer cmds[FRAMES];
   VkCommandBufferAllocateInfo cai = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
                                       .commandPool = pool, .commandBufferCount = FRAMES };
   CHECK(vkAllocateCommandBuffers(dev, &cai, cmds));
   VkSemaphore acq[FRAMES], *done = calloc(n, sizeof(VkSemaphore));
   VkFence fences[FRAMES];
   for (int i = 0; i < FRAMES; i++) {
      VkSemaphoreCreateInfo s = { .sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO };
      CHECK(vkCreateSemaphore(dev, &s, NULL, &acq[i]));
      VkFenceCreateInfo fc = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO,
                               .flags = VK_FENCE_CREATE_SIGNALED_BIT };
      CHECK(vkCreateFence(dev, &fc, NULL, &fences[i]));
   }
   for (uint32_t i = 0; i < n; i++) {
      VkSemaphoreCreateInfo s = { .sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO };
      CHECK(vkCreateSemaphore(dev, &s, NULL, &done[i]));
   }

   const double t0 = now_s();
   double next = t0, pres_sum = 0, pres_max = 0;
   uint64_t frame = 0;
   int suboptimal = 0;
   MSG msg;
   for (;;) {
      while (PeekMessageA(&msg, NULL, 0, 0, PM_REMOVE)) {
         if (msg.message == WM_QUIT) goto out;
         TranslateMessage(&msg);
         DispatchMessageA(&msg);
      }
      const double t = now_s();
      if (t - t0 >= seconds)
         break;
      if (cap > 0) {
         if (t < next) { Sleep((DWORD)((next - t) * 1000.0)); while (now_s() < next) ; }
         next += 1.0 / cap;
         if (now_s() > next + 0.1) next = now_s();
      }
      const int f = (int)(frame % FRAMES);
      CHECK(vkWaitForFences(dev, 1, &fences[f], VK_TRUE, UINT64_MAX));
      uint32_t idx;
      VkResult ar = vkAcquireNextImageKHR(dev, sc, UINT64_MAX, acq[f], VK_NULL_HANDLE, &idx);
      if (ar < 0) { fprintf(stderr, "acquire %d\n", ar); break; }
      CHECK(vkResetFences(dev, 1, &fences[f]));

      VkCommandBuffer cb = cmds[f];
      VkCommandBufferBeginInfo bi = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO };
      CHECK(vkBeginCommandBuffer(cb, &bi));
      VkImageMemoryBarrier b = { .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                                 .dstAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
                                 .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
                                 .newLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
                                 .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
                                 .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
                                 .image = imgs[idx],
                                 .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
      vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
                           VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT, 0, 0, NULL, 0, NULL, 1, &b);
      VkRenderingAttachmentInfo att = {
         .sType = VK_STRUCTURE_TYPE_RENDERING_ATTACHMENT_INFO, .imageView = views[idx],
         .imageLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
         .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR, .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
         .clearValue.color.float32 = { 0.05f, 0.08f, 0.20f, 1.0f } };
      VkRenderingInfo ri = { .sType = VK_STRUCTURE_TYPE_RENDERING_INFO,
                             .renderArea = { { 0, 0 }, ext }, .layerCount = 1,
                             .colorAttachmentCount = 1, .pColorAttachments = &att };
      vkCmdBeginRendering(cb, &ri);
      /* grid: 1-pixel lines every 32 pixels, two diagonals of steps */
      VkClearRect rects[512];
      uint32_t nr = 0;
      for (uint32_t x = 0; x < ext.width && nr < 200; x += 32)
         rects[nr++] = (VkClearRect){ { { (int32_t)x, 0 }, { 1, ext.height } }, 0, 1 };
      for (uint32_t y = 0; y < ext.height && nr < 400; y += 32)
         rects[nr++] = (VkClearRect){ { { 0, (int32_t)y }, { ext.width, 1 } }, 0, 1 };
      VkClearAttachment grey = { VK_IMAGE_ASPECT_COLOR_BIT, 0, { .color.float32 = { 0.5f, 0.5f, 0.5f, 1 } } };
      vkCmdClearAttachments(cb, 1, &grey, nr, rects);
      nr = 0;
      for (uint32_t s = 0; s < 96 && nr < 96; s++)
         rects[nr++] = (VkClearRect){ { { (int32_t)(ext.width / 8 + s * 3), (int32_t)(ext.height / 2 + s * 2) }, { 3, 2 } }, 0, 1 };
      VkClearAttachment yellow = { VK_IMAGE_ASPECT_COLOR_BIT, 0, { .color.float32 = { 1, 0.85f, 0.1f, 1 } } };
      vkCmdClearAttachments(cb, 1, &yellow, nr, rects);
      /* the moving box: `speed` widths per second, back and forth */
      const double tt = t - t0;
      const uint32_t box = ext.height / 8;
      const double span = (double)(ext.width - box);
      double ph = tt * speed * (double)ext.width / span;
      ph = ph - 2.0 * (double)(int)(ph / 2.0);
      const int32_t bx = (int32_t)((ph < 1.0 ? ph : 2.0 - ph) * span);
      VkClearRect mv = { { { bx, (int32_t)(ext.height / 4) }, { box, box } }, 0, 1 };
      VkClearAttachment white = { VK_IMAGE_ASPECT_COLOR_BIT, 0, { .color.float32 = { 1, 1, 1, 1 } } };
      vkCmdClearAttachments(cb, 1, &white, 1, &mv);
      /* static HUD box, and the frame counter in binary under it */
      VkClearRect hud = { { { 16, 16 }, { box * 2, box / 2 } }, 0, 1 };
      VkClearAttachment red = { VK_IMAGE_ASPECT_COLOR_BIT, 0, { .color.float32 = { 0.9f, 0.1f, 0.1f, 1 } } };
      vkCmdClearAttachments(cb, 1, &red, 1, &hud);
      nr = 0;
      for (int bit = 0; bit < 16; bit++)
         if (frame & (1ull << bit))
            rects[nr++] = (VkClearRect){ { { 16 + bit * 12, (int32_t)(24 + box / 2) }, { 10, 10 } }, 0, 1 };
      VkClearAttachment green = { VK_IMAGE_ASPECT_COLOR_BIT, 0, { .color.float32 = { 0.1f, 0.9f, 0.2f, 1 } } };
      if (nr)
         vkCmdClearAttachments(cb, 1, &green, nr, rects);
      vkCmdEndRendering(cb);
      b.srcAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT;
      b.dstAccessMask = 0;
      b.oldLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL;
      b.newLayout = VK_IMAGE_LAYOUT_PRESENT_SRC_KHR;
      vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
                           VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, 0, 0, NULL, 0, NULL, 1, &b);
      CHECK(vkEndCommandBuffer(cb));
      VkPipelineStageFlags ws = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT;
      VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .waitSemaphoreCount = 1,
                          .pWaitSemaphores = &acq[f], .pWaitDstStageMask = &ws,
                          .commandBufferCount = 1, .pCommandBuffers = &cb,
                          .signalSemaphoreCount = 1, .pSignalSemaphores = &done[idx] };
      CHECK(vkQueueSubmit(queue, 1, &si, fences[f]));
      VkPresentInfoKHR pi = { .sType = VK_STRUCTURE_TYPE_PRESENT_INFO_KHR,
                              .waitSemaphoreCount = 1, .pWaitSemaphores = &done[idx],
                              .swapchainCount = 1, .pSwapchains = &sc, .pImageIndices = &idx };
      const double p0 = now_s();
      VkResult pr = vkQueuePresentKHR(queue, &pi);
      const double pd_ = now_s() - p0;
      pres_sum += pd_;
      if (pd_ > pres_max) pres_max = pd_;
      if (pr == VK_SUBOPTIMAL_KHR) suboptimal++;
      else if (pr < 0) { fprintf(stderr, "present %d\n", pr); break; }
      frame++;
   }
out:
   vkDeviceWaitIdle(dev);
   {
      const double el = now_s() - t0;
      printf("frames %llu in %.2f s: %.1f fps; vkQueuePresentKHR avg %.3f ms, max %.3f ms; "
             "suboptimal %d\n", (unsigned long long)frame, el, frame / el,
             frame ? pres_sum / frame * 1000.0 : 0.0, pres_max * 1000.0, suboptimal);
   }
   vkDestroySwapchainKHR(dev, sc, NULL);
   vkDestroySurfaceKHR(instance, surface, NULL);
   DestroyWindow(hwnd);
   return 0;
}
