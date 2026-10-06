/* vk_bl_readback: does NVK lay out a block-linear swapchain-style image the
 * way its DRM format modifier says?  (Patch 0028: Win32 scanout swapchains
 * are block-linear, presented with NVIDIA's modifier, and the host's
 * display reads the memory by that modifier.)
 *
 * Builds the image the Win32 WSI builds: B8G8R8A8_UNORM, w x h,
 * VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT with one modifier (default
 * 0x0300000000606015: NVIDIA block-linear 2D, kind 0x06, GOB kind
 * generation 2, sector layout 1, block height 2^5 GOBs, what NVK picks for
 * the swapchain on GB20x), dedicated device-local memory.  NVK on RM does
 * not advertise VK_EXT_image_drm_format_modifier on Windows, but the WSI
 * uses that tiling internally and so does this test (no validation layers).
 *
 *   1. writes a coordinate pattern ((y << 16) | x, per pixel) with
 *      vkCmdCopyBufferToImage (copy engine),
 *   2. clears a rectangle that crosses GOB and block boundaries with
 *      vkCmdClearAttachments inside a render pass (3D engine, the image as
 *      a color target, as an application renders into a swapchain image),
 *   3. reads the memory's raw bytes back through a buffer bound to the same
 *      memory, and checks every pixel at the address the modifier gives it
 *      (NIL's TuringColor2D GOB: 64 B x 8 rows, as host_import_spike.c,
 *      where NVIDIA's own driver read this layout pixel-exact).
 *
 * The same raw bytes are what DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY exports, so
 * a pass means the display sees the picture NVK rendered, not a scramble.
 *
 *   vk_bl_readback [width=1920] [height=1080] [modifier=0x0300000000606015]
 *                  [read_as=modifier]
 *   modifier 0: LINEAR (control).  read_as: check the raw bytes as another
 *   modifier (negative control: a wrong block height must fail).
 */
#include <vulkan/vulkan.h>
#include <inttypes.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "vk_direct_driver.h"

#define CHECK(x) do { VkResult r_ = (x); \
   if (r_ != VK_SUCCESS) { fprintf(stderr, "FAIL %s:%d: %s -> %d\n", __FILE__, __LINE__, #x, r_); exit(1); } \
   } while (0)

#define BPP 4u
#define CLEAR_VALUE 0xff3366ccu /* B8G8R8A8: b=0xcc g=0x66 r=0x33 a=0xff */

/* Byte xb (0..63), row y (0..7) inside a TuringColor2D GOB (NIL copy.rs) */
static inline uint32_t gob_off(uint32_t xb, uint32_t y)
{
   return (xb / 32) * 256 + (y / 4) * 128 + ((xb % 32) / 16) * 64 +
          (y % 4) * 16 + (xb % 16);
}

/* Offset of pixel (x, y): block-linear 2D, blocks 1 GOB wide and 2^h GOBs
 * tall, row-major; or linear with `pitch` */
static inline uint64_t px_off(bool bl, uint32_t h, uint32_t pitch,
                              uint32_t x, uint32_t y)
{
   if (!bl)
      return (uint64_t)y * pitch + (uint64_t)x * BPP;
   const uint32_t xb = x * BPP;
   const uint32_t gobs_x = pitch / 64;
   const uint32_t bh = 8u << h;
   const uint64_t block = (uint64_t)(y / bh) * gobs_x + xb / 64;
   return block * (512u << h) + ((y % bh) / 8) * 512u + gob_off(xb % 64, y % 8);
}

static uint32_t find_type(VkPhysicalDevice pd, uint32_t bits, VkMemoryPropertyFlags want)
{
   VkPhysicalDeviceMemoryProperties mp;
   vkGetPhysicalDeviceMemoryProperties(pd, &mp);
   for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
      if ((bits & (1u << i)) && (mp.memoryTypes[i].propertyFlags & want) == want)
         return i;
   fprintf(stderr, "no memory type 0x%x with 0x%x\n", bits, want);
   exit(1);
}

static void make_buffer(VkPhysicalDevice pd, VkDevice dev, VkDeviceSize size,
                        VkBufferUsageFlags usage, VkBuffer *buf, VkDeviceMemory *mem,
                        void **map)
{
   VkBufferCreateInfo bci = { .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
      .size = size, .usage = usage, .sharingMode = VK_SHARING_MODE_EXCLUSIVE };
   CHECK(vkCreateBuffer(dev, &bci, NULL, buf));
   VkMemoryRequirements r;
   vkGetBufferMemoryRequirements(dev, *buf, &r);
   VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .allocationSize = r.size,
      .memoryTypeIndex = find_type(pd, r.memoryTypeBits,
                                   VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                                   VK_MEMORY_PROPERTY_HOST_COHERENT_BIT) };
   CHECK(vkAllocateMemory(dev, &mai, NULL, mem));
   CHECK(vkBindBufferMemory(dev, *buf, *mem, 0));
   CHECK(vkMapMemory(dev, *mem, 0, VK_WHOLE_SIZE, 0, map));
}

static void barrier(VkCommandBuffer cb, VkImage img, VkImageLayout from, VkImageLayout to,
                    VkAccessFlags src, VkAccessFlags dst)
{
   VkImageMemoryBarrier b = { .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
      .srcAccessMask = src, .dstAccessMask = dst, .oldLayout = from, .newLayout = to,
      .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
      .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .image = img,
      .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
   vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT,
                        VK_PIPELINE_STAGE_ALL_COMMANDS_BIT, 0, 0, NULL, 0, NULL, 1, &b);
}

int main(int argc, char **argv)
{
   const uint32_t W = argc > 1 ? (uint32_t)atoi(argv[1]) : 1920;
   const uint32_t H = argc > 2 ? (uint32_t)atoi(argv[2]) : 1080;
   const uint64_t mod = argc > 3 ? strtoull(argv[3], NULL, 0) : 0x0300000000606015ull;
   const bool bl = mod != 0;
   const uint64_t read_as = argc > 4 ? strtoull(argv[4], NULL, 0) : mod;
   const bool read_bl = read_as != 0;
   const uint32_t h = (uint32_t)(read_as & 0xf);
   /* Clear rectangle: odd edges, across GOBs (16 px wide, 8 rows) and blocks */
   const VkRect2D rect = { { 37, 13 }, { W / 2 + 3, H / 2 + 5 } };

   VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .pApplicationName = "vk_bl_readback", .apiVersion = VK_API_VERSION_1_3 };
   VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
      .pApplicationInfo = &app };
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
   printf("device: %s, %ux%u, modifier 0x%016" PRIx64 " (%s)\n", props.deviceName, W, H,
          mod, bl ? "block-linear" : "linear");

   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = 0, .queueCount = 1, .pQueuePriorities = &prio };
   VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
      .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci };
   VkDevice dev;
   CHECK(vkCreateDevice(pd, &dci, NULL, &dev));
   VkQueue q;
   vkGetDeviceQueue(dev, 0, 0, &q);

   /* The image, as the WSI makes it */
   VkImageDrmFormatModifierListCreateInfoEXT mods = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_LIST_CREATE_INFO_EXT,
      .drmFormatModifierCount = 1, .pDrmFormatModifiers = &mod };
   VkImageCreateInfo ii = { .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
      .pNext = bl ? &mods : NULL, .imageType = VK_IMAGE_TYPE_2D,
      .format = VK_FORMAT_B8G8R8A8_UNORM, .extent = { W, H, 1 }, .mipLevels = 1,
      .arrayLayers = 1, .samples = VK_SAMPLE_COUNT_1_BIT,
      .tiling = bl ? VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT : VK_IMAGE_TILING_LINEAR,
      .usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT |
               VK_IMAGE_USAGE_TRANSFER_SRC_BIT,
      .sharingMode = VK_SHARING_MODE_EXCLUSIVE, .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED };
   VkImage img;
   CHECK(vkCreateImage(dev, &ii, NULL, &img));
   VkMemoryRequirements ir;
   vkGetImageMemoryRequirements(dev, img, &ir);
   VkImageSubresource isr = { bl ? VK_IMAGE_ASPECT_MEMORY_PLANE_0_BIT_EXT
                                 : VK_IMAGE_ASPECT_COLOR_BIT, 0, 0 };
   VkSubresourceLayout lay;
   vkGetImageSubresourceLayout(dev, img, &isr, &lay);
   printf("image: %" PRIu64 " B, align %" PRIu64 ", rowPitch %" PRIu64 ", offset %" PRIu64
          ", size %" PRIu64 "\n", (uint64_t)ir.size, (uint64_t)ir.alignment,
          (uint64_t)lay.rowPitch, (uint64_t)lay.offset, (uint64_t)lay.size);
   const uint32_t pitch = (uint32_t)lay.rowPitch;

   VkMemoryDedicatedAllocateInfo ded = { .sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
      .image = img };
   VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .pNext = &ded, .allocationSize = ir.size,
      .memoryTypeIndex = find_type(pd, ir.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT) };
   VkDeviceMemory imem;
   CHECK(vkAllocateMemory(dev, &mai, NULL, &imem));
   CHECK(vkBindImageMemory(dev, img, imem, 0));

   /* The raw view: a buffer on the same memory (NVK does not police the
    * dedicated allocation; the buffer's own mapping has no kind, so it sees
    * the bytes as they lie in VRAM, as the display does) */
   VkBufferCreateInfo rbci = { .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
      .size = ir.size, .usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT,
      .sharingMode = VK_SHARING_MODE_EXCLUSIVE };
   VkBuffer raw;
   CHECK(vkCreateBuffer(dev, &rbci, NULL, &raw));
   VkMemoryRequirements rr;
   vkGetBufferMemoryRequirements(dev, raw, &rr);
   if (!(rr.memoryTypeBits & (1u << mai.memoryTypeIndex))) {
      fprintf(stderr, "a buffer cannot live in the image's memory type\n");
      return 1;
   }
   CHECK(vkBindBufferMemory(dev, raw, imem, 0));

   const VkDeviceSize pix_bytes = (VkDeviceSize)W * H * BPP;
   VkBuffer up, down, rawdown;
   VkDeviceMemory upm, downm, rawdownm;
   uint32_t *upp, *downp;
   uint8_t *rawp;
   make_buffer(pd, dev, pix_bytes, VK_BUFFER_USAGE_TRANSFER_SRC_BIT, &up, &upm, (void **)&upp);
   make_buffer(pd, dev, pix_bytes, VK_BUFFER_USAGE_TRANSFER_DST_BIT, &down, &downm, (void **)&downp);
   make_buffer(pd, dev, ir.size, VK_BUFFER_USAGE_TRANSFER_DST_BIT, &rawdown, &rawdownm, (void **)&rawp);
   for (uint32_t y = 0; y < H; y++)
      for (uint32_t x = 0; x < W; x++)
         upp[(size_t)y * W + x] = (y << 16) | x;
   memset(downp, 0, pix_bytes);
   memset(rawp, 0, ir.size);

   VkAttachmentDescription att = { .format = VK_FORMAT_B8G8R8A8_UNORM,
      .samples = VK_SAMPLE_COUNT_1_BIT, .loadOp = VK_ATTACHMENT_LOAD_OP_LOAD,
      .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
      .stencilLoadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE,
      .stencilStoreOp = VK_ATTACHMENT_STORE_OP_DONT_CARE,
      .initialLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
      .finalLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL };
   VkAttachmentReference aref = { 0, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL };
   VkSubpassDescription sub = { .pipelineBindPoint = VK_PIPELINE_BIND_POINT_GRAPHICS,
      .colorAttachmentCount = 1, .pColorAttachments = &aref };
   VkRenderPassCreateInfo rpci = { .sType = VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO,
      .attachmentCount = 1, .pAttachments = &att, .subpassCount = 1, .pSubpasses = &sub };
   VkRenderPass rp;
   CHECK(vkCreateRenderPass(dev, &rpci, NULL, &rp));
   VkImageViewCreateInfo vci = { .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
      .image = img, .viewType = VK_IMAGE_VIEW_TYPE_2D, .format = VK_FORMAT_B8G8R8A8_UNORM,
      .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
   VkImageView view;
   CHECK(vkCreateImageView(dev, &vci, NULL, &view));
   VkFramebufferCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO,
      .renderPass = rp, .attachmentCount = 1, .pAttachments = &view, .width = W,
      .height = H, .layers = 1 };
   VkFramebuffer fb;
   CHECK(vkCreateFramebuffer(dev, &fci, NULL, &fb));

   VkCommandPoolCreateInfo pci = { .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
      .queueFamilyIndex = 0 };
   VkCommandPool pool;
   CHECK(vkCreateCommandPool(dev, &pci, NULL, &pool));
   VkCommandBufferAllocateInfo cai = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
      .commandPool = pool, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1 };
   VkCommandBuffer cb;
   CHECK(vkAllocateCommandBuffers(dev, &cai, &cb));
   VkCommandBufferBeginInfo cbi = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO };
   CHECK(vkBeginCommandBuffer(cb, &cbi));

   VkBufferImageCopy bic = { .imageSubresource = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1 },
      .imageExtent = { W, H, 1 } };
   barrier(cb, img, VK_IMAGE_LAYOUT_UNDEFINED, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, 0,
           VK_ACCESS_TRANSFER_WRITE_BIT);
   vkCmdCopyBufferToImage(cb, up, img, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, 1, &bic);
   barrier(cb, img, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
           VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL, VK_ACCESS_TRANSFER_WRITE_BIT,
           VK_ACCESS_COLOR_ATTACHMENT_READ_BIT | VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT);

   VkRenderPassBeginInfo rbi = { .sType = VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO,
      .renderPass = rp, .framebuffer = fb, .renderArea = { { 0, 0 }, { W, H } } };
   vkCmdBeginRenderPass(cb, &rbi, VK_SUBPASS_CONTENTS_INLINE);
   VkClearAttachment ca = { .aspectMask = VK_IMAGE_ASPECT_COLOR_BIT, .colorAttachment = 0,
      .clearValue.color.float32 = { 0x33 / 255.0f, 0x66 / 255.0f, 0xcc / 255.0f, 1.0f } };
   VkClearRect cr = { .rect = rect, .baseArrayLayer = 0, .layerCount = 1 };
   vkCmdClearAttachments(cb, 1, &ca, 1, &cr);
   vkCmdEndRenderPass(cb);

   /* Through the image (NVK's own view of its layout) and raw */
   vkCmdCopyImageToBuffer(cb, img, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, down, 1, &bic);
   VkMemoryBarrier mb = { .sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
      .srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT | VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
      .dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT };
   vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT,
                        VK_PIPELINE_STAGE_ALL_COMMANDS_BIT, 0, 1, &mb, 0, NULL, 0, NULL);
   VkBufferCopy bc = { 0, 0, ir.size };
   vkCmdCopyBuffer(cb, raw, rawdown, 1, &bc);
   CHECK(vkEndCommandBuffer(cb));
   VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1,
      .pCommandBuffers = &cb };
   CHECK(vkQueueSubmit(q, 1, &si, VK_NULL_HANDLE));
   CHECK(vkQueueWaitIdle(q));

   uint64_t bad_img = 0, bad_raw = 0, in_rect = 0;
   for (uint32_t y = 0; y < H; y++) {
      for (uint32_t x = 0; x < W; x++) {
         const bool r = x >= (uint32_t)rect.offset.x && x < rect.offset.x + rect.extent.width &&
                        y >= (uint32_t)rect.offset.y && y < rect.offset.y + rect.extent.height;
         const uint32_t want = r ? CLEAR_VALUE : ((y << 16) | x);
         in_rect += r;
         const uint32_t got_img = downp[(size_t)y * W + x];
         uint32_t got_raw;
         memcpy(&got_raw, rawp + lay.offset + px_off(read_bl, h, pitch, x, y), 4);
         if (got_img != want && bad_img++ < 5)
            printf("  image (%u,%u): 0x%08x, want 0x%08x\n", x, y, got_img, want);
         if (got_raw != want && bad_raw++ < 5)
            printf("  raw   (%u,%u): 0x%08x, want 0x%08x\n", x, y, got_raw, want);
      }
   }
   if (read_as != mod)
      printf("raw bytes read as modifier 0x%016" PRIx64 "\n", read_as);
   printf("checked %u pixels (%" PRIu64 " cleared by the 3D engine): through the image "
          "%" PRIu64 " wrong, raw bytes at the modifier's addresses %" PRIu64 " wrong\n",
          W * H, in_rect, bad_img, bad_raw);
   const bool ok = bad_img == 0 && bad_raw == 0;
   printf("%s\n", ok ? "PASS" : "FAIL");
   return ok ? 0 : 1;
}
