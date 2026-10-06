/* Offscreen rendering test, no WSI: a render pass clears a 256x256
 * R8G8B8A8_UNORM optimal-tiling (device-local, block-linear on NVK) image to
 * blue and draws one triangle with red/green/blue corners, the image is
 * copied into a host-visible buffer and checked on the CPU: corners are the
 * clear color, the triangle's centroid is a mix of all three, pixels next to
 * each vertex are dominated by that vertex's color. Optionally writes the
 * frame as a PPM. Renders `frames` times (one submit + fence wait each).
 *
 *   glslangValidator -V triangle.vert -o triangle.vert.spv
 *   glslangValidator -V triangle.frag -o triangle.frag.spv
 *   vk_offscreen_test [frames] [out.ppm]   (the .spv files in the cwd)
 */
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>

#include "vk_direct_driver.h"

#define CHECK(x) do { VkResult r_ = (x); \
   if (r_ != VK_SUCCESS) { fprintf(stderr, "FAIL %s:%d: %s -> %d\n", __FILE__, __LINE__, #x, r_); exit(1); } \
   } while (0)
#define STEP(s) do { fprintf(stderr, "step: %s\n", s); } while (0)

#define W 256
#define H 256

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

static uint32_t find_mem(const VkPhysicalDeviceMemoryProperties *mp, uint32_t bits,
                         VkMemoryPropertyFlags want)
{
   for (uint32_t i = 0; i < mp->memoryTypeCount; i++)
      if ((bits & (1u << i)) && (mp->memoryTypes[i].propertyFlags & want) == want)
         return i;
   fprintf(stderr, "no memory type for bits 0x%x flags 0x%x\n", bits, want);
   exit(1);
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

static const uint8_t *px(const uint8_t *img, int x, int y)
{
   return img + ((size_t)y * W + x) * 4;
}

int main(int argc, char **argv)
{
   int frames = argc > 1 ? atoi(argv[1]) : 1;
   const char *ppm = argc > 2 ? argv[2] : NULL;
   if (frames < 1)
      frames = 1;

   STEP("vkCreateInstance");
   VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .pApplicationName = "vk_offscreen_test", .apiVersion = VK_API_VERSION_1_3 };
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
   fprintf(stderr, "device: %s\n", props.deviceName);
   VkPhysicalDeviceMemoryProperties mp;
   vkGetPhysicalDeviceMemoryProperties(pd, &mp);

   uint32_t nqf = 0;
   vkGetPhysicalDeviceQueueFamilyProperties(pd, &nqf, NULL);
   VkQueueFamilyProperties qfp[16];
   if (nqf > 16) nqf = 16;
   vkGetPhysicalDeviceQueueFamilyProperties(pd, &nqf, qfp);
   uint32_t qf = UINT32_MAX;
   for (uint32_t i = 0; i < nqf; i++)
      if (qfp[i].queueFlags & VK_QUEUE_GRAPHICS_BIT) { qf = i; break; }
   if (qf == UINT32_MAX) { fprintf(stderr, "no graphics queue\n"); return 1; }

   STEP("vkCreateDevice");
   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = qf, .queueCount = 1, .pQueuePriorities = &prio };
   VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
      .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci };
   VkDevice dev;
   CHECK(vkCreateDevice(pd, &dci, NULL, &dev));
   VkQueue q;
   vkGetDeviceQueue(dev, qf, 0, &q);

   STEP("color image");
   const VkFormat fmt = VK_FORMAT_R8G8B8A8_UNORM;
   VkImageCreateInfo imci = { .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
      .imageType = VK_IMAGE_TYPE_2D, .format = fmt, .extent = { W, H, 1 },
      .mipLevels = 1, .arrayLayers = 1, .samples = VK_SAMPLE_COUNT_1_BIT,
      .tiling = VK_IMAGE_TILING_OPTIMAL,
      .usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT,
      .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED };
   VkImage img;
   CHECK(vkCreateImage(dev, &imci, NULL, &img));
   VkMemoryRequirements mr;
   vkGetImageMemoryRequirements(dev, img, &mr);
   VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .allocationSize = mr.size,
      .memoryTypeIndex = find_mem(&mp, mr.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT) };
   VkDeviceMemory imem;
   CHECK(vkAllocateMemory(dev, &mai, NULL, &imem));
   CHECK(vkBindImageMemory(dev, img, imem, 0));
   VkImageViewCreateInfo ivci = { .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
      .image = img, .viewType = VK_IMAGE_VIEW_TYPE_2D, .format = fmt,
      .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
   VkImageView view;
   CHECK(vkCreateImageView(dev, &ivci, NULL, &view));

   STEP("readback buffer");
   const VkDeviceSize rb_size = (VkDeviceSize)W * H * 4;
   VkBufferCreateInfo bci = { .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
      .size = rb_size, .usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT,
      .sharingMode = VK_SHARING_MODE_EXCLUSIVE };
   VkBuffer rb;
   CHECK(vkCreateBuffer(dev, &bci, NULL, &rb));
   vkGetBufferMemoryRequirements(dev, rb, &mr);
   mai.allocationSize = mr.size;
   mai.memoryTypeIndex = find_mem(&mp, mr.memoryTypeBits,
      VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT);
   VkDeviceMemory rbmem;
   CHECK(vkAllocateMemory(dev, &mai, NULL, &rbmem));
   CHECK(vkBindBufferMemory(dev, rb, rbmem, 0));
   uint8_t *rbptr;
   CHECK(vkMapMemory(dev, rbmem, 0, VK_WHOLE_SIZE, 0, (void **)&rbptr));
   memset(rbptr, 0xcd, rb_size);

   STEP("render pass + pipeline");
   VkAttachmentDescription att = { .format = fmt, .samples = VK_SAMPLE_COUNT_1_BIT,
      .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR, .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
      .stencilLoadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE,
      .stencilStoreOp = VK_ATTACHMENT_STORE_OP_DONT_CARE,
      .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
      .finalLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL };
   VkAttachmentReference aref = { 0, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL };
   VkSubpassDescription sub = { .pipelineBindPoint = VK_PIPELINE_BIND_POINT_GRAPHICS,
      .colorAttachmentCount = 1, .pColorAttachments = &aref };
   VkSubpassDependency deps = { .srcSubpass = 0, .dstSubpass = VK_SUBPASS_EXTERNAL,
      .srcStageMask = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
      .dstStageMask = VK_PIPELINE_STAGE_TRANSFER_BIT,
      .srcAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
      .dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT };
   VkRenderPassCreateInfo rpci = { .sType = VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO,
      .attachmentCount = 1, .pAttachments = &att, .subpassCount = 1, .pSubpasses = &sub,
      .dependencyCount = 1, .pDependencies = &deps };
   VkRenderPass rp;
   CHECK(vkCreateRenderPass(dev, &rpci, NULL, &rp));
   VkFramebufferCreateInfo fbci = { .sType = VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO,
      .renderPass = rp, .attachmentCount = 1, .pAttachments = &view,
      .width = W, .height = H, .layers = 1 };
   VkFramebuffer fb;
   CHECK(vkCreateFramebuffer(dev, &fbci, NULL, &fb));

   VkShaderModule vs = load_shader(dev, "triangle.vert.spv");
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
   VkViewport vp = { 0, 0, W, H, 0, 1 };
   VkRect2D sc = { { 0, 0 }, { W, H } };
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
   VkPipelineLayoutCreateInfo plci = { .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO };
   VkPipelineLayout pl;
   CHECK(vkCreatePipelineLayout(dev, &plci, NULL, &pl));
   VkGraphicsPipelineCreateInfo gpci = { .sType = VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO,
      .stageCount = 2, .pStages = stages, .pVertexInputState = &vi,
      .pInputAssemblyState = &ia, .pViewportState = &vps, .pRasterizationState = &rs,
      .pMultisampleState = &ms, .pColorBlendState = &cb, .layout = pl,
      .renderPass = rp, .subpass = 0 };
   VkPipeline pipe;
   CHECK(vkCreateGraphicsPipelines(dev, VK_NULL_HANDLE, 1, &gpci, NULL, &pipe));

   STEP("record");
   VkCommandPoolCreateInfo cpci = { .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
      .queueFamilyIndex = qf };
   VkCommandPool pool;
   CHECK(vkCreateCommandPool(dev, &cpci, NULL, &pool));
   VkCommandBufferAllocateInfo cbai = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
      .commandPool = pool, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1 };
   VkCommandBuffer cmd;
   CHECK(vkAllocateCommandBuffers(dev, &cbai, &cmd));
   VkCommandBufferBeginInfo cbbi = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO };
   CHECK(vkBeginCommandBuffer(cmd, &cbbi));
   VkClearValue clear = { .color = { .float32 = { 0.0f, 0.0f, 0.25f, 1.0f } } };
   VkRenderPassBeginInfo rpbi = { .sType = VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO,
      .renderPass = rp, .framebuffer = fb, .renderArea = sc,
      .clearValueCount = 1, .pClearValues = &clear };
   vkCmdBeginRenderPass(cmd, &rpbi, VK_SUBPASS_CONTENTS_INLINE);
   vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, pipe);
   vkCmdDraw(cmd, 3, 1, 0, 0);
   vkCmdEndRenderPass(cmd);
   VkBufferImageCopy region = { .bufferOffset = 0,
      .imageSubresource = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1 },
      .imageExtent = { W, H, 1 } };
   vkCmdCopyImageToBuffer(cmd, img, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, rb, 1, &region);
   VkMemoryBarrier hb = { .sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
      .srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT, .dstAccessMask = VK_ACCESS_HOST_READ_BIT };
   vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_HOST_BIT, 0,
                        1, &hb, 0, NULL, 0, NULL);
   CHECK(vkEndCommandBuffer(cmd));

   STEP("submit");
   VkFenceCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
   VkFence fence;
   CHECK(vkCreateFence(dev, &fci, NULL, &fence));
   for (int f = 0; f < frames; f++) {
      VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
         .commandBufferCount = 1, .pCommandBuffers = &cmd };
      CHECK(vkQueueSubmit(q, 1, &si, fence));
      VkResult wr = vkWaitForFences(dev, 1, &fence, VK_TRUE, 10ull * 1000 * 1000 * 1000);
      if (wr != VK_SUCCESS) {
         fprintf(stderr, "FAIL: fence wait frame %d -> %d\n", f, wr);
         return 1;
      }
      CHECK(vkResetFences(dev, 1, &fence));
   }

   STEP("verify");
   int bad = 0;
   const uint8_t clear_px[4] = { 0, 0, 64, 255 };
   const int corners[4][2] = { { 2, 2 }, { W - 3, 2 }, { 2, H - 3 }, { W - 3, H - 3 } };
   for (int i = 0; i < 4; i++) {
      const uint8_t *p = px(rbptr, corners[i][0], corners[i][1]);
      for (int c = 0; c < 4; c++)
         if (abs((int)p[c] - (int)clear_px[c]) > 1) {
            fprintf(stderr, "corner (%d,%d) = %u %u %u %u, want clear %u %u %u %u\n",
                    corners[i][0], corners[i][1], p[0], p[1], p[2], p[3],
                    clear_px[0], clear_px[1], clear_px[2], clear_px[3]);
            bad++;
            break;
         }
   }
   /* Vertices (see triangle.vert): top (red) (W/2, H/8), bottom left (green)
    * (W/8, 7H/8), bottom right (blue) (7W/8, 7H/8). Sample near each.
    */
   struct { int x, y, ch; } vtx[3] = {
      { W / 2, H / 8 + 6, 0 }, { W / 8 + 8, 7 * H / 8 - 3, 1 }, { 7 * W / 8 - 8, 7 * H / 8 - 3, 2 },
   };
   for (int i = 0; i < 3; i++) {
      const uint8_t *p = px(rbptr, vtx[i].x, vtx[i].y);
      int ch = vtx[i].ch;
      if (p[ch] < 200 || p[(ch + 1) % 3] > 60 || p[(ch + 2) % 3] > 60 || p[3] != 255) {
         fprintf(stderr, "vertex %d pixel (%d,%d) = %u %u %u %u, want channel %d dominant\n",
                 i, vtx[i].x, vtx[i].y, p[0], p[1], p[2], p[3], ch);
         bad++;
      }
   }
   const int cx = W / 2, cy = (H / 8 + 7 * H / 8 + 7 * H / 8) / 3;
   const uint8_t *p = px(rbptr, cx, cy);
   for (int c = 0; c < 3; c++)
      if (p[c] < 60 || p[c] > 110) {
         fprintf(stderr, "centroid (%d,%d) = %u %u %u %u, want ~85 each\n",
                 cx, cy, p[0], p[1], p[2], p[3]);
         bad++;
         break;
      }
   size_t covered = 0;
   for (int y = 0; y < H; y++)
      for (int x = 0; x < W; x++) {
         const uint8_t *q2 = px(rbptr, x, y);
         if (!(q2[0] == clear_px[0] && q2[1] == clear_px[1] && q2[2] == clear_px[2]))
            covered++;
      }
   /* The triangle is (3/4 W x 3/4 H) / 2 = 28.1% of the image */
   fprintf(stderr, "covered %zu of %d pixels (%.1f%%, expect ~28.1%%)\n", covered, W * H,
           100.0 * covered / (W * H));
   if (covered < (size_t)(W * H * 0.26) || covered > (size_t)(W * H * 0.30))
      bad++;

   if (ppm) {
      FILE *f = fopen(ppm, "wb");
      if (f) {
         fprintf(f, "P6\n%d %d\n255\n", W, H);
         for (int i = 0; i < W * H; i++)
            fwrite(rbptr + i * 4, 1, 3, f);
         fclose(f);
         fprintf(stderr, "wrote %s\n", ppm);
      }
   }

   vkDeviceWaitIdle(dev);
   vkDestroyFence(dev, fence, NULL);
   vkDestroyCommandPool(dev, pool, NULL);
   vkDestroyPipeline(dev, pipe, NULL);
   vkDestroyPipelineLayout(dev, pl, NULL);
   vkDestroyShaderModule(dev, vs, NULL);
   vkDestroyShaderModule(dev, fs, NULL);
   vkDestroyFramebuffer(dev, fb, NULL);
   vkDestroyRenderPass(dev, rp, NULL);
   vkDestroyImageView(dev, view, NULL);
   vkDestroyImage(dev, img, NULL);
   vkFreeMemory(dev, imem, NULL);
   vkUnmapMemory(dev, rbmem);
   vkDestroyBuffer(dev, rb, NULL);
   vkFreeMemory(dev, rbmem, NULL);
   vkDestroyDevice(dev, NULL);
   vkDestroyInstance(inst, NULL);

   if (bad) {
      fprintf(stderr, "FAIL: %d check(s) failed (%d frame(s))\n", bad, frames);
      return 1;
   }
   printf("PASS: offscreen triangle %dx%d, %d frame(s)\n", W, H, frames);
   return 0;
}
