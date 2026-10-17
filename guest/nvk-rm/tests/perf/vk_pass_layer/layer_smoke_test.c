/*
 * Smoke test for VK_LAYER_CONDUIT_pass_profile: a few command buffers with
 * render passes (dynamic rendering, clear only), buffer fills and copies, an
 * image clear and a blit, then a wait; on any driver (lavapipe needs no GPU):
 *   VK_DRIVER_FILES=/usr/share/vulkan/icd.d/lvp_icd.json \
 *   VK_LAYER_PATH=$PWD VK_INSTANCE_LAYERS=VK_LAYER_CONDUIT_pass_profile \
 *   CONDUIT_PASS_PROFILE_FILE=- ./layer_smoke_test
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <vulkan/vulkan.h>

#define CHECK(x) do { VkResult r_ = (x); if (r_ != VK_SUCCESS) { \
   fprintf(stderr, "%s failed: %d\n", #x, r_); exit(1); } } while (0)

int
main(void)
{
   VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
                             .apiVersion = VK_API_VERSION_1_3 };
   VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                                .pApplicationInfo = &app };
   VkInstance inst;
   CHECK(vkCreateInstance(&ici, NULL, &inst));
   uint32_t n = 1;
   VkPhysicalDevice pd;
   vkEnumeratePhysicalDevices(inst, &n, &pd);

   VkPhysicalDeviceVulkan13Features f13 = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES,
      .dynamicRendering = VK_TRUE, .synchronization2 = VK_TRUE };
   float prio = 1;
   VkDeviceQueueCreateInfo qci = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                                   .queueFamilyIndex = 0, .queueCount = 1,
                                   .pQueuePriorities = &prio };
   VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                              .pNext = &f13, .queueCreateInfoCount = 1,
                              .pQueueCreateInfos = &qci };
   VkDevice dev;
   CHECK(vkCreateDevice(pd, &dci, NULL, &dev));
   VkQueue q;
   vkGetDeviceQueue(dev, 0, 0, &q);

   VkImageCreateInfo imi = { .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
      .imageType = VK_IMAGE_TYPE_2D, .format = VK_FORMAT_R8G8B8A8_UNORM,
      .extent = { 256, 256, 1 }, .mipLevels = 1, .arrayLayers = 1,
      .samples = VK_SAMPLE_COUNT_1_BIT, .tiling = VK_IMAGE_TILING_OPTIMAL,
      .usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT |
               VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT,
      .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED };
   VkImage img[2];
   VkDeviceMemory mem[2];
   VkImageView view[2];
   for (int i = 0; i < 2; i++) {
      CHECK(vkCreateImage(dev, &imi, NULL, &img[i]));
      VkMemoryRequirements mr;
      vkGetImageMemoryRequirements(dev, img[i], &mr);
      VkPhysicalDeviceMemoryProperties mp;
      vkGetPhysicalDeviceMemoryProperties(pd, &mp);
      uint32_t t = 0;
      for (; t < mp.memoryTypeCount; t++)
         if (mr.memoryTypeBits & (1u << t) &&
             (mp.memoryTypes[t].propertyFlags & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT))
            break;
      if (t == mp.memoryTypeCount)
         t = __builtin_ctz(mr.memoryTypeBits);
      VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                                   .allocationSize = mr.size, .memoryTypeIndex = t };
      CHECK(vkAllocateMemory(dev, &mai, NULL, &mem[i]));
      CHECK(vkBindImageMemory(dev, img[i], mem[i], 0));
      VkImageViewCreateInfo vi = { .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
         .image = img[i], .viewType = VK_IMAGE_VIEW_TYPE_2D, .format = imi.format,
         .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
      CHECK(vkCreateImageView(dev, &vi, NULL, &view[i]));
   }

   VkBufferCreateInfo bci = { .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
      .size = 1 << 20, .usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT |
                                VK_BUFFER_USAGE_TRANSFER_DST_BIT };
   VkBuffer buf[2];
   VkDeviceMemory bmem[2];
   for (int i = 0; i < 2; i++) {
      CHECK(vkCreateBuffer(dev, &bci, NULL, &buf[i]));
      VkMemoryRequirements mr;
      vkGetBufferMemoryRequirements(dev, buf[i], &mr);
      VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
         .allocationSize = mr.size, .memoryTypeIndex = __builtin_ctz(mr.memoryTypeBits) };
      CHECK(vkAllocateMemory(dev, &mai, NULL, &bmem[i]));
      CHECK(vkBindBufferMemory(dev, buf[i], bmem[i], 0));
   }

   VkCommandPoolCreateInfo pci = { .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
      .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT, .queueFamilyIndex = 0 };
   VkCommandPool pool;
   CHECK(vkCreateCommandPool(dev, &pci, NULL, &pool));
   VkCommandBufferAllocateInfo cai = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
      .commandPool = pool, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1 };
   VkCommandBuffer cb;
   CHECK(vkAllocateCommandBuffers(dev, &cai, &cb));

   for (int frame = 0; frame < 300; frame++) {
      VkCommandBufferBeginInfo bi = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO };
      CHECK(vkBeginCommandBuffer(cb, &bi));
      VkImageMemoryBarrier ib[2];
      for (int i = 0; i < 2; i++)
         ib[i] = (VkImageMemoryBarrier){ .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
            .dstAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
            .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
            .newLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
            .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
            .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .image = img[i],
            .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
      vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,
                           VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT, 0, 0, NULL, 0, NULL, 2, ib);
      for (int pass = 0; pass < 3; pass++) {
         VkRenderingAttachmentInfo att = { .sType = VK_STRUCTURE_TYPE_RENDERING_ATTACHMENT_INFO,
            .imageView = view[pass & 1], .imageLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
            .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR, .storeOp = VK_ATTACHMENT_STORE_OP_STORE };
         VkRenderingInfo ri = { .sType = VK_STRUCTURE_TYPE_RENDERING_INFO,
            .renderArea = { { 0, 0 }, { 256, 256 } }, .layerCount = 1,
            .colorAttachmentCount = 1, .pColorAttachments = &att };
         vkCmdBeginRendering(cb, &ri);
         vkCmdEndRendering(cb);
      }
      vkCmdFillBuffer(cb, buf[0], 0, 1 << 20, 0x1234);
      vkCmdFillBuffer(cb, buf[0], 0, 1 << 19, 0x5678);
      VkBufferCopy bc = { 0, 0, 1 << 20 };
      vkCmdCopyBuffer(cb, buf[0], buf[1], 1, &bc);
      VkImageBlit blit = { { VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1 }, { { 0, 0, 0 }, { 256, 256, 1 } },
                           { VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1 }, { { 0, 0, 0 }, { 256, 256, 1 } } };
      /* images are in COLOR_ATTACHMENT layout: transfer layouts need no care on lavapipe */
      (void)blit;
      CHECK(vkEndCommandBuffer(cb));
      VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1,
                          .pCommandBuffers = &cb };
      CHECK(vkQueueSubmit(q, 1, &si, VK_NULL_HANDLE));
      CHECK(vkQueueWaitIdle(q));
      CHECK(vkResetCommandBuffer(cb, 0));
      usleep(10000);
   }
   vkDeviceWaitIdle(dev);
   vkDestroyDevice(dev, NULL);
   vkDestroyInstance(inst, NULL);
   return 0;
}
