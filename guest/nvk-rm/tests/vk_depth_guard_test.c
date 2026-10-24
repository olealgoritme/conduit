/* Depth/stencil clears and readback, with guard buffers in the same memory. */
#include <windows.h>
#include <vulkan/vulkan.h>
#include <stdint.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "vk_direct_driver.h"

#define CHECK(x) do { VkResult r_ = (x); if (r_ != VK_SUCCESS) { \
   fprintf(stderr, "FAIL %s:%d: %s -> %d\n", __FILE__, __LINE__, #x, r_); exit(1); } } while (0)
#define GUARD_SIZE 65536

static VkDevice dev;
static VkPhysicalDeviceMemoryProperties memprops;
static VkDeviceSize granularity;
static VkQueue queue;
static VkCommandPool pool;

struct allocation {
   void *base;
   size_t size;
};

static VKAPI_ATTR void *VKAPI_CALL
host_alloc(void *data, size_t size, size_t alignment, VkSystemAllocationScope scope)
{
   (void)data;
   (void)scope;
   if (alignment < sizeof(void *))
      alignment = sizeof(void *);
   void *base = malloc(size + alignment + sizeof(struct allocation) + 16);
   if (!base)
      return NULL;
   uintptr_t address = (uintptr_t)base + sizeof(struct allocation);
   address = (address + alignment - 1) & ~(uintptr_t)(alignment - 1);
   /* An 8-byte request must also work without incidental 16-byte alignment. */
   if (alignment < 16 && address % 16 == 0)
      address += alignment;
   struct allocation *a = (struct allocation *)address - 1;
   *a = (struct allocation){base, size};
   return (void *)address;
}

static VKAPI_ATTR void VKAPI_CALL
host_free(void *data, void *ptr)
{
   (void)data;
   if (ptr)
      free(((struct allocation *)ptr - 1)->base);
}

static VKAPI_ATTR void *VKAPI_CALL
host_realloc(void *data, void *ptr, size_t size, size_t alignment,
             VkSystemAllocationScope scope)
{
   if (!size) {
      host_free(data, ptr);
      return NULL;
   }
   void *next = host_alloc(data, size, alignment, scope);
   if (next && ptr) {
      size_t old_size = ((struct allocation *)ptr - 1)->size;
      memcpy(next, ptr, old_size < size ? old_size : size);
      host_free(data, ptr);
   }
   return next;
}

static const VkAllocationCallbacks host_allocator = {
   .pfnAllocation = host_alloc,
   .pfnReallocation = host_realloc,
   .pfnFree = host_free,
};

static VkDeviceSize align_up(VkDeviceSize value, VkDeviceSize alignment)
{
   return (value + alignment - 1) / alignment * alignment;
}

static uint32_t memory_type(uint32_t bits, VkMemoryPropertyFlags flags)
{
   for (uint32_t i = 0; i < memprops.memoryTypeCount; i++)
      if ((bits & (1u << i)) && (memprops.memoryTypes[i].propertyFlags & flags) == flags)
         return i;
   fprintf(stderr, "FAIL no memory type\n");
   exit(1);
}

static VkBuffer buffer(VkDeviceSize size, VkBufferUsageFlags usage,
                        VkMemoryRequirements *req)
{
   VkBufferCreateInfo ci = { VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
      .size = size, .usage = usage };
   VkBuffer b;
   CHECK(vkCreateBuffer(dev, &ci, NULL, &b));
   vkGetBufferMemoryRequirements(dev, b, req);
   return b;
}

static unsigned run(VkFormat format, uint32_t w, uint32_t h,
                    VkSampleCountFlagBits samples)
{
   const bool stencil = format != VK_FORMAT_D32_SFLOAT;
   VkImageAspectFlags aspects = VK_IMAGE_ASPECT_DEPTH_BIT |
      (stencil ? VK_IMAGE_ASPECT_STENCIL_BIT : 0);
   VkImageCreateInfo ici = { VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
      .imageType = VK_IMAGE_TYPE_2D, .format = format, .extent = {w, h, 1},
      .mipLevels = 1, .arrayLayers = 1, .samples = samples,
      .tiling = VK_IMAGE_TILING_OPTIMAL,
      .usage = VK_IMAGE_USAGE_DEPTH_STENCIL_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT };
   VkImage image;
   CHECK(vkCreateImage(dev, &ici, NULL, &image));
   VkMemoryRequirements mr, br;
   vkGetImageMemoryRequirements(dev, image, &mr);
   VkBuffer guards[2];
   for (unsigned i = 0; i < 2; i++)
      guards[i] = buffer(GUARD_SIZE, VK_BUFFER_USAGE_TRANSFER_SRC_BIT |
                         VK_BUFFER_USAGE_TRANSFER_DST_BIT, &br);
   VkDeviceSize alignment = mr.alignment > granularity ? mr.alignment : granularity;
   if (br.alignment > alignment)
      alignment = br.alignment;
   VkDeviceSize image_offset = align_up(br.size, alignment);
   VkDeviceSize tail_offset = align_up(image_offset + mr.size, alignment);
   VkMemoryAllocateInfo ai = { VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .allocationSize = tail_offset + br.size,
      .memoryTypeIndex = memory_type(mr.memoryTypeBits & br.memoryTypeBits,
                                    VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT) };
   VkDeviceMemory memory;
   CHECK(vkAllocateMemory(dev, &ai, NULL, &memory));
   CHECK(vkBindBufferMemory(dev, guards[0], memory, 0));
   CHECK(vkBindBufferMemory(dev, guards[1], memory, tail_offset));
   VkResult bind = vkBindImageMemory(dev, image, memory, image_offset);
   if (bind != VK_SUCCESS) {
      printf("FAIL format=%u %ux%u samples=%u bind=%d\n", format, w, h, samples, bind);
      vkDestroyImage(dev, image, NULL);
      for (unsigned i = 0; i < 2; i++)
         vkDestroyBuffer(dev, guards[i], NULL);
      vkFreeMemory(dev, memory, NULL);
      return 1;
   }
   VkDeviceSize depth_offset = 2 * GUARD_SIZE;
   VkDeviceSize stencil_offset = depth_offset + (VkDeviceSize)w * h * 4;
   VkDeviceSize read_size = stencil_offset + (VkDeviceSize)w * h;
   VkBuffer readback = buffer(read_size, VK_BUFFER_USAGE_TRANSFER_DST_BIT, &br);
   ai.allocationSize = br.size;
   ai.memoryTypeIndex = memory_type(br.memoryTypeBits,
      VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT);
   VkDeviceMemory read_memory;
   CHECK(vkAllocateMemory(dev, &ai, NULL, &read_memory));
   CHECK(vkBindBufferMemory(dev, readback, read_memory, 0));
   uint8_t *mapped;
   CHECK(vkMapMemory(dev, read_memory, 0, VK_WHOLE_SIZE, 0, (void **)&mapped));
   memset(mapped, 0xcd, (size_t)read_size);
   VkImageViewCreateInfo vci = { VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
      .image = image, .viewType = VK_IMAGE_VIEW_TYPE_2D, .format = format,
      .subresourceRange = {aspects, 0, 1, 0, 1} };
   VkImageView view;
   CHECK(vkCreateImageView(dev, &vci, NULL, &view));
   VkAttachmentDescription attachment = { .format = format, .samples = samples,
      .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR, .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
      .stencilLoadOp = VK_ATTACHMENT_LOAD_OP_CLEAR, .stencilStoreOp = VK_ATTACHMENT_STORE_OP_STORE,
      .initialLayout = VK_IMAGE_LAYOUT_DEPTH_STENCIL_ATTACHMENT_OPTIMAL,
      .finalLayout = VK_IMAGE_LAYOUT_DEPTH_STENCIL_ATTACHMENT_OPTIMAL };
   VkAttachmentReference depth = {0, VK_IMAGE_LAYOUT_DEPTH_STENCIL_ATTACHMENT_OPTIMAL};
   VkSubpassDescription subpass = { .pipelineBindPoint = VK_PIPELINE_BIND_POINT_GRAPHICS,
      .pDepthStencilAttachment = &depth };
   VkRenderPassCreateInfo rpci = { VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO,
      .attachmentCount = 1, .pAttachments = &attachment,
      .subpassCount = 1, .pSubpasses = &subpass };
   VkRenderPass pass;
   CHECK(vkCreateRenderPass(dev, &rpci, NULL, &pass));
   VkFramebufferCreateInfo fbci = { VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO,
      .renderPass = pass, .attachmentCount = 1, .pAttachments = &view,
      .width = w, .height = h, .layers = 1 };
   VkFramebuffer fb;
   CHECK(vkCreateFramebuffer(dev, &fbci, NULL, &fb));
   VkCommandBufferAllocateInfo cai = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
      .commandPool = pool, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1 };
   VkCommandBuffer cb;
   CHECK(vkAllocateCommandBuffers(dev, &cai, &cb));
   VkCommandBufferBeginInfo begin = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO };
   CHECK(vkBeginCommandBuffer(cb, &begin));
   const uint32_t patterns[2] = {0x13579bdf, 0x2468ace0};
   for (unsigned i = 0; i < 2; i++)
      vkCmdFillBuffer(cb, guards[i], 0, GUARD_SIZE, patterns[i]);
   VkImageMemoryBarrier ib = { VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
      .dstAccessMask = VK_ACCESS_DEPTH_STENCIL_ATTACHMENT_WRITE_BIT,
      .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
      .newLayout = VK_IMAGE_LAYOUT_DEPTH_STENCIL_ATTACHMENT_OPTIMAL,
      .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
      .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
      .image = image, .subresourceRange = {aspects, 0, 1, 0, 1} };
   VkMemoryBarrier mb = { VK_STRUCTURE_TYPE_MEMORY_BARRIER,
      .srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
      .dstAccessMask = VK_ACCESS_MEMORY_READ_BIT | VK_ACCESS_MEMORY_WRITE_BIT };
   vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT,
                        0, 1, &mb, 0, NULL, 1, &ib);
   VkClearValue clear = {.depthStencil = {0.25f, 0x39}};
   VkRenderPassBeginInfo rpbegin = { VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO,
      .renderPass = pass, .framebuffer = fb, .renderArea = {{0, 0}, {w, h}},
      .clearValueCount = 1, .pClearValues = &clear };
   vkCmdBeginRenderPass(cb, &rpbegin, VK_SUBPASS_CONTENTS_INLINE);
   VkClearAttachment ca = {.aspectMask = aspects, .clearValue.depthStencil = {0.75f, 0xa7}};
   VkClearRect rect = {{{0, 0}, {w, h}}, 0, 1};
   vkCmdClearAttachments(cb, 1, &ca, 1, &rect);
   vkCmdEndRenderPass(cb);
   ib.srcAccessMask = VK_ACCESS_DEPTH_STENCIL_ATTACHMENT_WRITE_BIT;
   ib.dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT;
   ib.oldLayout = VK_IMAGE_LAYOUT_DEPTH_STENCIL_ATTACHMENT_OPTIMAL;
   ib.newLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL;
   mb.srcAccessMask = VK_ACCESS_MEMORY_WRITE_BIT;
   mb.dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT;
   vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT,
                        0, 1, &mb, 0, NULL, 1, &ib);
   if (samples == VK_SAMPLE_COUNT_1_BIT) {
      VkBufferImageCopy regions[2] = {
         {.bufferOffset = depth_offset, .imageSubresource = {VK_IMAGE_ASPECT_DEPTH_BIT, 0, 0, 1},
          .imageExtent = {w, h, 1}},
         {.bufferOffset = stencil_offset, .imageSubresource = {VK_IMAGE_ASPECT_STENCIL_BIT, 0, 0, 1},
          .imageExtent = {w, h, 1}},
      };
      vkCmdCopyImageToBuffer(cb, image, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
                            readback, stencil ? 2 : 1, regions);
   }
   for (unsigned i = 0; i < 2; i++) {
      VkBufferCopy copy = {0, i * GUARD_SIZE, GUARD_SIZE};
      vkCmdCopyBuffer(cb, guards[i], readback, 1, &copy);
   }
   mb.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
   mb.dstAccessMask = VK_ACCESS_HOST_READ_BIT;
   vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_HOST_BIT,
                        0, 1, &mb, 0, NULL, 0, NULL);
   CHECK(vkEndCommandBuffer(cb));
   VkFenceCreateInfo fci = { VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
   VkFence fence;
   CHECK(vkCreateFence(dev, &fci, NULL, &fence));
   VkSubmitInfo submit = { VK_STRUCTURE_TYPE_SUBMIT_INFO,
      .commandBufferCount = 1, .pCommandBuffers = &cb };
   CHECK(vkQueueSubmit(queue, 1, &submit, fence));
   CHECK(vkWaitForFences(dev, 1, &fence, VK_TRUE, 10000000000ull));
   unsigned bad_guards = 0, bad_depth = 0, bad_stencil = 0;
   for (unsigned i = 0; i < 2; i++)
      for (unsigned j = 0; j < GUARD_SIZE / 4; j++)
         bad_guards += ((uint32_t *)(mapped + i * GUARD_SIZE))[j] != patterns[i];
   if (samples == VK_SAMPLE_COUNT_1_BIT) {
      for (uint32_t i = 0; i < w * h; i++) {
         uint32_t value = ((uint32_t *)(mapped + depth_offset))[i];
         if (format == VK_FORMAT_D24_UNORM_S8_UINT)
            bad_depth += (value & 0xffffff) < 0xbffffe || (value & 0xffffff) > 0xc00000;
         else
            bad_depth += value != 0x3f400000;
         if (stencil)
            bad_stencil += mapped[stencil_offset + i] != 0xa7;
      }
   }
   unsigned failed = bad_guards || bad_depth || bad_stencil;
   printf("%s format=%u %ux%u samples=%u guards=%u depth=%u stencil=%u\n",
          failed ? "FAIL" : "PASS", format, w, h, samples,
          bad_guards, bad_depth, bad_stencil);
   vkDestroyFence(dev, fence, NULL);
   vkFreeCommandBuffers(dev, pool, 1, &cb);
   vkDestroyFramebuffer(dev, fb, NULL);
   vkDestroyRenderPass(dev, pass, NULL);
   vkDestroyImageView(dev, view, NULL);
   vkDestroyImage(dev, image, NULL);
   for (unsigned i = 0; i < 2; i++)
      vkDestroyBuffer(dev, guards[i], NULL);
   vkFreeMemory(dev, memory, NULL);
   vkUnmapMemory(dev, read_memory);
   vkDestroyBuffer(dev, readback, NULL);
   vkFreeMemory(dev, read_memory, NULL);
   return failed;
}

int main(int argc, char **argv)
{
   setvbuf(stdout, NULL, _IOLBF, 0);
   const VkAllocationCallbacks *allocator =
      argc > 1 && !strcmp(argv[1], "strict") ? &host_allocator : NULL;
   VkApplicationInfo app = { VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .pApplicationName = "vk_depth_guard_test", .apiVersion = VK_API_VERSION_1_1 };
   VkInstanceCreateInfo ici = { VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
      .pApplicationInfo = &app };
   direct_driver_chain(&ici);
   VkInstance inst;
   CHECK(vkCreateInstance(&ici, NULL, &inst));
   uint32_t count = 16;
   VkPhysicalDevice devices[16], pd = VK_NULL_HANDLE;
   CHECK(vkEnumeratePhysicalDevices(inst, &count, devices));
   VkPhysicalDeviceProperties props;
   for (uint32_t i = 0; i < count; i++) {
      vkGetPhysicalDeviceProperties(devices[i], &props);
      if (strstr(props.deviceName, "NVK")) {
         pd = devices[i];
         break;
      }
   }
   if (!pd)
      return 1;
   printf("DEVICE=%s\n", props.deviceName);
   granularity = props.limits.bufferImageGranularity;
   vkGetPhysicalDeviceMemoryProperties(pd, &memprops);
   uint32_t nq = 32, family;
   VkQueueFamilyProperties queues[32];
   vkGetPhysicalDeviceQueueFamilyProperties(pd, &nq, queues);
   for (family = 0; family < nq; family++)
      if (queues[family].queueFlags & VK_QUEUE_GRAPHICS_BIT)
         break;
   float priority = 1;
   VkDeviceQueueCreateInfo qci = { VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = family, .queueCount = 1, .pQueuePriorities = &priority };
   VkDeviceCreateInfo dci = { VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
      .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci };
   CHECK(vkCreateDevice(pd, &dci, NULL, &dev));
   vkGetDeviceQueue(dev, family, 0, &queue);
   VkCommandPoolCreateInfo pci = { VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
      .queueFamilyIndex = family };
   CHECK(vkCreateCommandPool(dev, &pci, allocator, &pool));
   const VkFormat formats[] = {VK_FORMAT_D32_SFLOAT_S8_UINT,
      VK_FORMAT_D32_SFLOAT, VK_FORMAT_D24_UNORM_S8_UINT};
   const uint32_t shapes[][2] = {{63, 65}, {960, 540}, {961, 541}};
   unsigned tested = 0, failed = 0;
   for (unsigned f = 0; f < 3; f++)
      for (unsigned s = 0; s < 3; s++)
         for (unsigned samples = 1; samples <= 4; samples *= 2) {
            VkImageFormatProperties p;
            CHECK(vkGetPhysicalDeviceImageFormatProperties(pd, formats[f], VK_IMAGE_TYPE_2D,
               VK_IMAGE_TILING_OPTIMAL, VK_IMAGE_USAGE_DEPTH_STENCIL_ATTACHMENT_BIT |
               VK_IMAGE_USAGE_TRANSFER_SRC_BIT, 0, &p));
            if (!(p.sampleCounts & samples))
               continue;
            failed += run(formats[f], shapes[s][0], shapes[s][1], samples);
            tested++;
         }
   vkDestroyCommandPool(dev, pool, allocator);
   vkDestroyDevice(dev, NULL);
   vkDestroyInstance(inst, NULL);
   printf("RESULT tested=%u failed=%u\n", tested, failed);
   return failed != 0;
}
