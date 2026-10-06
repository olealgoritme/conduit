/* Present test for the Win32 WSI: a spinning triangle in a swapchain, for
 * `seconds`. With NVK on RM under Conduit's Helios adapter the WSI presents
 * by ScanoutFlip (patch 0021), so the frames appear on the guest's scanout
 * (the viewer) whatever owns the desktop; the window is never shown and can
 * live on an invisible desktop (an ssh session). Elsewhere it is an ordinary
 * swapchain on a hidden window.
 *
 *   glslangValidator -V spin.vert -o spin.vert.spv
 *   glslangValidator -V triangle.frag -o triangle.frag.spv
 *   vk_scanout_present [seconds=30] [width=1920] [height=1080] [images=3]
 *   (the .spv files in the cwd)
 *
 * Prints frames and fps every 5 s; exits non-zero on any Vulkan error.
 */
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <math.h>

#ifdef _WIN32
#include <windows.h>
#include <vulkan/vulkan_win32.h>
#endif

#include "vk_direct_driver.h"

#define CHECK(x) do { VkResult r_ = (x); \
   if (r_ != VK_SUCCESS) { fprintf(stderr, "FAIL %s:%d: %s -> %d\n", __FILE__, __LINE__, #x, r_); exit(1); } \
   } while (0)
#define STEP(s) do { fprintf(stderr, "step: %s\n", s); } while (0)

#define MAX_IMAGES 8
#define IN_FLIGHT 2

#ifndef _WIN32
int main(void)
{
   printf("vk_scanout_present: Windows only (VK_KHR_win32_surface)\n");
   return 77;
}
#else

static uint32_t *read_file(const char *path, size_t *size)
{
   FILE *f = fopen(path, "rb");
   if (!f) { perror(path); exit(1); }
   fseek(f, 0, SEEK_END);
   *size = ftell(f);
   fseek(f, 0, SEEK_SET);
   uint32_t *buf = malloc(*size);
   if (fread(buf, 1, *size, f) != *size) { perror("fread"); exit(1); }
   fclose(f);
   return buf;
}

static VkShaderModule load_shader(VkDevice dev, const char *path)
{
   size_t size;
   uint32_t *code = read_file(path, &size);
   VkShaderModuleCreateInfo ci = { .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
      .codeSize = size, .pCode = code };
   VkShaderModule m;
   CHECK(vkCreateShaderModule(dev, &ci, NULL, &m));
   free(code);
   return m;
}

static double now_s(void)
{
   LARGE_INTEGER f, c;
   QueryPerformanceFrequency(&f);
   QueryPerformanceCounter(&c);
   return (double)c.QuadPart / (double)f.QuadPart;
}

static HWND make_window(uint32_t w, uint32_t h)
{
   WNDCLASSA wc = { .lpfnWndProc = DefWindowProcA, .hInstance = GetModuleHandleA(NULL),
      .lpszClassName = "vk_scanout_present" };
   RegisterClassA(&wc);
   /* A borderless popup: the client area is the whole window, w x h. Never
    * shown: the scanout flip does not need it on screen. */
   HWND hwnd = CreateWindowExA(0, wc.lpszClassName, "vk_scanout_present", WS_POPUP,
                               0, 0, (int)w, (int)h, NULL, NULL, wc.hInstance, NULL);
   if (!hwnd) {
      fprintf(stderr, "CreateWindowEx failed: %lu\n", (unsigned long)GetLastError());
      exit(1);
   }
   return hwnd;
}

int main(int argc, char **argv)
{
   const double seconds = argc > 1 ? atof(argv[1]) : 30.0;
   uint32_t want_w = argc > 2 ? (uint32_t)atoi(argv[2]) : 1920;
   uint32_t want_h = argc > 3 ? (uint32_t)atoi(argv[3]) : 1080;
   uint32_t want_images = argc > 4 ? (uint32_t)atoi(argv[4]) : 3;

   HWND hwnd = make_window(want_w, want_h);

   STEP("vkCreateInstance");
   const char *inst_exts[] = { VK_KHR_SURFACE_EXTENSION_NAME, VK_KHR_WIN32_SURFACE_EXTENSION_NAME };
   VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .pApplicationName = "vk_scanout_present", .apiVersion = VK_API_VERSION_1_3 };
   VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
      .pApplicationInfo = &app, .enabledExtensionCount = 2, .ppEnabledExtensionNames = inst_exts };
   direct_driver_chain(&ici);
   VkInstance inst;
   CHECK(vkCreateInstance(&ici, NULL, &inst));

   uint32_t npd = 1;
   VkPhysicalDevice pd;
   VkResult er = vkEnumeratePhysicalDevices(inst, &npd, &pd);
   if ((er != VK_SUCCESS && er != VK_INCOMPLETE) || npd == 0) {
      fprintf(stderr, "no physical devices (%d)\n", er);
      return 1;
   }
   VkPhysicalDeviceProperties props;
   vkGetPhysicalDeviceProperties(pd, &props);
   fprintf(stderr, "device: %s\n", props.deviceName);

   STEP("vkCreateWin32SurfaceKHR");
   VkWin32SurfaceCreateInfoKHR sci = { .sType = VK_STRUCTURE_TYPE_WIN32_SURFACE_CREATE_INFO_KHR,
      .hinstance = GetModuleHandleA(NULL), .hwnd = hwnd };
   VkSurfaceKHR surface;
   CHECK(vkCreateWin32SurfaceKHR(inst, &sci, NULL, &surface));

   uint32_t nqf = 0;
   vkGetPhysicalDeviceQueueFamilyProperties(pd, &nqf, NULL);
   VkQueueFamilyProperties qfp[16];
   if (nqf > 16) nqf = 16;
   vkGetPhysicalDeviceQueueFamilyProperties(pd, &nqf, qfp);
   uint32_t qf = UINT32_MAX;
   for (uint32_t i = 0; i < nqf; i++) {
      VkBool32 present = VK_FALSE;
      vkGetPhysicalDeviceSurfaceSupportKHR(pd, i, surface, &present);
      if ((qfp[i].queueFlags & VK_QUEUE_GRAPHICS_BIT) && present) { qf = i; break; }
   }
   if (qf == UINT32_MAX) { fprintf(stderr, "no graphics queue that can present\n"); return 1; }

   STEP("vkCreateDevice");
   const char *dev_exts[] = { VK_KHR_SWAPCHAIN_EXTENSION_NAME };
   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = qf, .queueCount = 1, .pQueuePriorities = &prio };
   VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
      .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci,
      .enabledExtensionCount = 1, .ppEnabledExtensionNames = dev_exts };
   VkDevice dev;
   CHECK(vkCreateDevice(pd, &dci, NULL, &dev));
   VkQueue q;
   vkGetDeviceQueue(dev, qf, 0, &q);

   STEP("swapchain");
   VkSurfaceCapabilitiesKHR caps;
   CHECK(vkGetPhysicalDeviceSurfaceCapabilitiesKHR(pd, surface, &caps));
   VkExtent2D extent = caps.currentExtent;
   if (extent.width == UINT32_MAX)
      extent = (VkExtent2D) { want_w, want_h };
   uint32_t nfmt = 0;
   CHECK(vkGetPhysicalDeviceSurfaceFormatsKHR(pd, surface, &nfmt, NULL));
   VkSurfaceFormatKHR fmts[16];
   if (nfmt > 16) nfmt = 16;
   CHECK(vkGetPhysicalDeviceSurfaceFormatsKHR(pd, surface, &nfmt, fmts));
   VkSurfaceFormatKHR sf = fmts[0];
   for (uint32_t i = 0; i < nfmt; i++)
      if (fmts[i].format == VK_FORMAT_B8G8R8A8_UNORM) { sf = fmts[i]; break; }
   uint32_t nimg = want_images < caps.minImageCount ? caps.minImageCount : want_images;
   if (caps.maxImageCount && nimg > caps.maxImageCount) nimg = caps.maxImageCount;
   if (nimg > MAX_IMAGES) nimg = MAX_IMAGES;
   VkSwapchainCreateInfoKHR swci = { .sType = VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR,
      .surface = surface, .minImageCount = nimg, .imageFormat = sf.format,
      .imageColorSpace = sf.colorSpace, .imageExtent = extent, .imageArrayLayers = 1,
      .imageUsage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT,
      .imageSharingMode = VK_SHARING_MODE_EXCLUSIVE,
      .preTransform = VK_SURFACE_TRANSFORM_IDENTITY_BIT_KHR,
      .compositeAlpha = VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR,
      .presentMode = VK_PRESENT_MODE_FIFO_KHR, .clipped = VK_TRUE };
   VkSwapchainKHR swapchain;
   CHECK(vkCreateSwapchainKHR(dev, &swci, NULL, &swapchain));
   VkImage images[MAX_IMAGES];
   nimg = MAX_IMAGES;
   CHECK(vkGetSwapchainImagesKHR(dev, swapchain, &nimg, images));
   fprintf(stderr, "swapchain: %u images %ux%u format %d\n", nimg, extent.width,
           extent.height, sf.format);

   STEP("render pass + pipeline");
   VkAttachmentDescription att = { .format = sf.format, .samples = VK_SAMPLE_COUNT_1_BIT,
      .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR, .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
      .stencilLoadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE,
      .stencilStoreOp = VK_ATTACHMENT_STORE_OP_DONT_CARE,
      .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
      .finalLayout = VK_IMAGE_LAYOUT_PRESENT_SRC_KHR };
   VkAttachmentReference aref = { 0, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL };
   VkSubpassDescription sub = { .pipelineBindPoint = VK_PIPELINE_BIND_POINT_GRAPHICS,
      .colorAttachmentCount = 1, .pColorAttachments = &aref };
   VkSubpassDependency dep = { .srcSubpass = VK_SUBPASS_EXTERNAL, .dstSubpass = 0,
      .srcStageMask = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
      .dstStageMask = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
      .dstAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT };
   VkRenderPassCreateInfo rpci = { .sType = VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO,
      .attachmentCount = 1, .pAttachments = &att, .subpassCount = 1, .pSubpasses = &sub,
      .dependencyCount = 1, .pDependencies = &dep };
   VkRenderPass rp;
   CHECK(vkCreateRenderPass(dev, &rpci, NULL, &rp));

   VkImageView views[MAX_IMAGES];
   VkFramebuffer fbs[MAX_IMAGES];
   VkSemaphore render_done[MAX_IMAGES];
   VkSemaphoreCreateInfo semci = { .sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO };
   for (uint32_t i = 0; i < nimg; i++) {
      VkImageViewCreateInfo ivci = { .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
         .image = images[i], .viewType = VK_IMAGE_VIEW_TYPE_2D, .format = sf.format,
         .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
      CHECK(vkCreateImageView(dev, &ivci, NULL, &views[i]));
      VkFramebufferCreateInfo fbci = { .sType = VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO,
         .renderPass = rp, .attachmentCount = 1, .pAttachments = &views[i],
         .width = extent.width, .height = extent.height, .layers = 1 };
      CHECK(vkCreateFramebuffer(dev, &fbci, NULL, &fbs[i]));
      CHECK(vkCreateSemaphore(dev, &semci, NULL, &render_done[i]));
   }

   VkShaderModule vs = load_shader(dev, "spin.vert.spv");
   VkShaderModule fs = load_shader(dev, "triangle.frag.spv");
   VkPipelineShaderStageCreateInfo stages[2] = {
      { .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
        .stage = VK_SHADER_STAGE_VERTEX_BIT, .module = vs, .pName = "main" },
      { .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
        .stage = VK_SHADER_STAGE_FRAGMENT_BIT, .module = fs, .pName = "main" },
   };
   VkPipelineVertexInputStateCreateInfo vi = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO };
   VkPipelineInputAssemblyStateCreateInfo ia = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
      .topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST };
   VkViewport vp = { 0, 0, (float)extent.width, (float)extent.height, 0, 1 };
   VkRect2D sc = { { 0, 0 }, extent };
   VkPipelineViewportStateCreateInfo vps = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO,
      .viewportCount = 1, .pViewports = &vp, .scissorCount = 1, .pScissors = &sc };
   VkPipelineRasterizationStateCreateInfo rs = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO,
      .polygonMode = VK_POLYGON_MODE_FILL, .cullMode = VK_CULL_MODE_NONE,
      .frontFace = VK_FRONT_FACE_COUNTER_CLOCKWISE, .lineWidth = 1.0f };
   VkPipelineMultisampleStateCreateInfo ms = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO,
      .rasterizationSamples = VK_SAMPLE_COUNT_1_BIT };
   VkPipelineColorBlendAttachmentState cba = { .colorWriteMask = 0xf };
   VkPipelineColorBlendStateCreateInfo cb = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO,
      .attachmentCount = 1, .pAttachments = &cba };
   VkPushConstantRange pcr = { VK_SHADER_STAGE_VERTEX_BIT, 0, 8 };
   VkPipelineLayoutCreateInfo plci = { .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
      .pushConstantRangeCount = 1, .pPushConstantRanges = &pcr };
   VkPipelineLayout pl;
   CHECK(vkCreatePipelineLayout(dev, &plci, NULL, &pl));
   VkGraphicsPipelineCreateInfo gpci = { .sType = VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO,
      .stageCount = 2, .pStages = stages, .pVertexInputState = &vi,
      .pInputAssemblyState = &ia, .pViewportState = &vps, .pRasterizationState = &rs,
      .pMultisampleState = &ms, .pColorBlendState = &cb, .layout = pl,
      .renderPass = rp, .subpass = 0 };
   VkPipeline pipe;
   CHECK(vkCreateGraphicsPipelines(dev, VK_NULL_HANDLE, 1, &gpci, NULL, &pipe));

   VkCommandPoolCreateInfo cpci = { .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
      .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT, .queueFamilyIndex = qf };
   VkCommandPool pool;
   CHECK(vkCreateCommandPool(dev, &cpci, NULL, &pool));
   VkCommandBuffer cmds[IN_FLIGHT];
   VkCommandBufferAllocateInfo cbai = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
      .commandPool = pool, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
      .commandBufferCount = IN_FLIGHT };
   CHECK(vkAllocateCommandBuffers(dev, &cbai, cmds));
   VkSemaphore acquired[IN_FLIGHT];
   VkFence fences[IN_FLIGHT];
   VkFenceCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO,
      .flags = VK_FENCE_CREATE_SIGNALED_BIT };
   for (int i = 0; i < IN_FLIGHT; i++) {
      CHECK(vkCreateSemaphore(dev, &semci, NULL, &acquired[i]));
      CHECK(vkCreateFence(dev, &fci, NULL, &fences[i]));
   }

   STEP("present loop");
   const double t0 = now_s();
   double next_report = t0 + 5.0, last_report = t0;
   uint64_t frames = 0, frames_at_report = 0;
   for (;;) {
      const double t = now_s();
      if (t - t0 >= seconds)
         break;
      const int f = (int)(frames % IN_FLIGHT);
      CHECK(vkWaitForFences(dev, 1, &fences[f], VK_TRUE, 10ull * 1000 * 1000 * 1000));
      CHECK(vkResetFences(dev, 1, &fences[f]));

      uint32_t idx;
      CHECK(vkAcquireNextImageKHR(dev, swapchain, 5ull * 1000 * 1000 * 1000, acquired[f],
                                  VK_NULL_HANDLE, &idx));

      VkCommandBuffer cmd = cmds[f];
      CHECK(vkResetCommandBuffer(cmd, 0));
      VkCommandBufferBeginInfo cbbi = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
         .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT };
      CHECK(vkBeginCommandBuffer(cmd, &cbbi));
      /* A slowly cycling background, so a stuck frame is easy to see */
      const float tt = (float)(t - t0);
      VkClearValue clear = { .color = { .float32 = {
         0.10f + 0.10f * sinf(tt * 0.7f), 0.10f, 0.20f + 0.10f * cosf(tt * 0.5f), 1.0f } } };
      VkRenderPassBeginInfo rpbi = { .sType = VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO,
         .renderPass = rp, .framebuffer = fbs[idx], .renderArea = sc,
         .clearValueCount = 1, .pClearValues = &clear };
      vkCmdBeginRenderPass(cmd, &rpbi, VK_SUBPASS_CONTENTS_INLINE);
      vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, pipe);
      const float push[2] = { tt * 1.5f, (float)extent.height / (float)extent.width };
      vkCmdPushConstants(cmd, pl, VK_SHADER_STAGE_VERTEX_BIT, 0, sizeof(push), push);
      vkCmdDraw(cmd, 3, 1, 0, 0);
      vkCmdEndRenderPass(cmd);
      CHECK(vkEndCommandBuffer(cmd));

      VkPipelineStageFlags wait_stage = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT;
      VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
         .waitSemaphoreCount = 1, .pWaitSemaphores = &acquired[f],
         .pWaitDstStageMask = &wait_stage,
         .commandBufferCount = 1, .pCommandBuffers = &cmd,
         .signalSemaphoreCount = 1, .pSignalSemaphores = &render_done[idx] };
      CHECK(vkQueueSubmit(q, 1, &si, fences[f]));

      VkPresentInfoKHR pi = { .sType = VK_STRUCTURE_TYPE_PRESENT_INFO_KHR,
         .waitSemaphoreCount = 1, .pWaitSemaphores = &render_done[idx],
         .swapchainCount = 1, .pSwapchains = &swapchain, .pImageIndices = &idx };
      CHECK(vkQueuePresentKHR(q, &pi));
      frames++;

      const double tn = now_s();
      if (tn >= next_report) {
         printf("%6.1f s: %llu frames, %.1f fps\n", tn - t0, (unsigned long long)frames,
                (double)(frames - frames_at_report) / (tn - last_report));
         fflush(stdout);
         frames_at_report = frames;
         last_report = tn;
         next_report += 5.0;
      }
   }
   const double total = now_s() - t0;

   STEP("teardown");
   CHECK(vkDeviceWaitIdle(dev));
   for (int i = 0; i < IN_FLIGHT; i++) {
      vkDestroySemaphore(dev, acquired[i], NULL);
      vkDestroyFence(dev, fences[i], NULL);
   }
   vkDestroyCommandPool(dev, pool, NULL);
   vkDestroyPipeline(dev, pipe, NULL);
   vkDestroyPipelineLayout(dev, pl, NULL);
   vkDestroyShaderModule(dev, vs, NULL);
   vkDestroyShaderModule(dev, fs, NULL);
   for (uint32_t i = 0; i < nimg; i++) {
      vkDestroySemaphore(dev, render_done[i], NULL);
      vkDestroyFramebuffer(dev, fbs[i], NULL);
      vkDestroyImageView(dev, views[i], NULL);
   }
   vkDestroyRenderPass(dev, rp, NULL);
   vkDestroySwapchainKHR(dev, swapchain, NULL);
   vkDestroyDevice(dev, NULL);
   vkDestroySurfaceKHR(inst, surface, NULL);
   vkDestroyInstance(inst, NULL);
   DestroyWindow(hwnd);

   printf("PASS: %llu frames presented in %.1f s (%.1f fps), %ux%u, %u images\n",
          (unsigned long long)frames, total, frames / total, extent.width, extent.height, nimg);
   return 0;
}
#endif
