/* vk_texture_roundtrip: texture upload, copy-back and sampling round trip.
 *
 * Images of several formats (RGBA8, R8, BC1, BC3, BC5, BC7) and shapes
 * (square, wide, tall, non-power-of-two, arrays, full mip chains) are
 * sub-allocated from one device-local memory, as DXVK does, and filled
 * per mip with deterministic bytes through vkCmdCopyBufferToImage.  Then:
 *  - every mip is copied back with vkCmdCopyImageToBuffer and compared with
 *    the source bytes;
 *  - a compute shader reads a 32x32 grid of every mip four ways (texelFetch
 *    and nearest textureLod through the full view, and through a one-mip
 *    view), which must agree, and the results are hashed per image so that
 *    a run on another driver for the same GPU can be compared.
 * A few MiB of data, a few dozen tiny dispatches.
 */
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>

#include "texture_read_comp.h"

#define CHECK(x) do { VkResult r_ = (x); if (r_ != VK_SUCCESS) { \
   fprintf(stderr, "FAIL %s:%d: %s -> %d\n", __FILE__, __LINE__, #x, r_); exit(1); } } while (0)

struct fmt { VkFormat f; const char *name; uint32_t bw, bh, bytes; };
static const struct fmt fmts[] = {
   { VK_FORMAT_R8G8B8A8_UNORM, "RGBA8", 1, 1, 4 },
   { VK_FORMAT_R8_UNORM, "R8", 1, 1, 1 },
   { VK_FORMAT_BC1_RGBA_UNORM_BLOCK, "BC1", 4, 4, 8 },
   { VK_FORMAT_BC3_UNORM_BLOCK, "BC3", 4, 4, 16 },
   { VK_FORMAT_BC5_UNORM_BLOCK, "BC5", 4, 4, 16 },
   { VK_FORMAT_BC7_UNORM_BLOCK, "BC7", 4, 4, 16 },
};
struct shape { uint32_t w, h, layers; };
static const struct shape shapes[] = {
   { 256, 256, 1 }, { 512, 128, 1 }, { 128, 512, 1 }, { 1024, 64, 1 }, { 64, 1024, 1 },
   { 100, 60, 1 }, { 256, 64, 3 }, { 8, 8, 1 },
};
#define NF (sizeof(fmts) / sizeof(fmts[0]))
#define NS (sizeof(shapes) / sizeof(shapes[0]))
#define NIMG (NF * NS)
#define MAXMIP 11

static VkDevice dev;
static VkPhysicalDeviceMemoryProperties memprops;

static uint32_t find_mem(uint32_t bits, VkMemoryPropertyFlags want)
{
   for (uint32_t i = 0; i < memprops.memoryTypeCount; i++)
      if ((bits & (1u << i)) && (memprops.memoryTypes[i].propertyFlags & want) == want)
         return i;
   fprintf(stderr, "FAIL no memory type\n");
   exit(1);
}

static uint32_t mipdim(uint32_t d, uint32_t m) { d >>= m; return d ? d : 1; }
static uint32_t nmips(uint32_t w, uint32_t h)
{
   uint32_t n = 1, d = w > h ? w : h;
   while (d > 1) { d >>= 1; n++; }
   return n > MAXMIP ? MAXMIP : n;
}

static uint64_t fnv(uint64_t h, const void *p, size_t n)
{
   const uint8_t *b = p;
   for (size_t i = 0; i < n; i++) { h ^= b[i]; h *= 0x100000001b3ull; }
   return h;
}

struct img {
   const struct fmt *fmt;
   const struct shape *sh;
   uint32_t mips;
   VkImage image;
   VkDeviceSize mem_off;
   VkImageView full, per_mip[MAXMIP];
   VkDeviceSize src_off[MAXMIP], src_size[MAXMIP];   /* in staging/readback, all layers */
   uint32_t out_base[MAXMIP][4];
};

int main(int argc, char **argv)
{
   const char *want = argc > 1 ? argv[1] : "NVK";
   VkApplicationInfo app = { VK_STRUCTURE_TYPE_APPLICATION_INFO, .apiVersion = VK_API_VERSION_1_3 };
   VkInstanceCreateInfo ici = { VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app };
   VkInstance inst;
   CHECK(vkCreateInstance(&ici, NULL, &inst));
   uint32_t npd = 8;
   VkPhysicalDevice pds[8], pd = VK_NULL_HANDLE;
   CHECK(vkEnumeratePhysicalDevices(inst, &npd, pds));
   VkPhysicalDeviceProperties props;
   for (uint32_t i = 0; i < npd; i++) {
      vkGetPhysicalDeviceProperties(pds[i], &props);
      if (!pd && strstr(props.deviceName, want))
         pd = pds[i];
   }
   if (!pd) {
      fprintf(stderr, "FAIL no device matching '%s'\n", want);
      return 1;
   }
   vkGetPhysicalDeviceProperties(pd, &props);
   vkGetPhysicalDeviceMemoryProperties(pd, &memprops);
   printf("using: %s\n", props.deviceName);

   uint32_t qf = 0, nqf = 16;
   VkQueueFamilyProperties qfp[16];
   vkGetPhysicalDeviceQueueFamilyProperties(pd, &nqf, qfp);
   for (qf = 0; qf < nqf; qf++)
      if (qfp[qf].queueFlags & VK_QUEUE_GRAPHICS_BIT)
         break;
   VkPhysicalDeviceFeatures2 f2 = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2 };
   f2.features.textureCompressionBC = VK_TRUE;
   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = { VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = qf, .queueCount = 1, .pQueuePriorities = &prio };
   VkDeviceCreateInfo dci = { VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, &f2,
      .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci };
   CHECK(vkCreateDevice(pd, &dci, NULL, &dev));
   VkQueue q;
   vkGetDeviceQueue(dev, qf, 0, &q);

   /* Images, then one memory for all of them */
   static struct img imgs[NIMG];
   VkDeviceSize mem_size = 0, staging_size = 0;
   uint32_t mem_bits = ~0u, out_slots = 0;
   for (uint32_t i = 0; i < NIMG; i++) {
      struct img *im = &imgs[i];
      im->fmt = &fmts[i / NS];
      im->sh = &shapes[i % NS];
      im->mips = nmips(im->sh->w, im->sh->h);
      VkImageCreateInfo ii = { VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO, .imageType = VK_IMAGE_TYPE_2D,
         .format = im->fmt->f, .extent = { im->sh->w, im->sh->h, 1 }, .mipLevels = im->mips,
         .arrayLayers = im->sh->layers, .samples = VK_SAMPLE_COUNT_1_BIT,
         .tiling = VK_IMAGE_TILING_OPTIMAL,
         .usage = VK_IMAGE_USAGE_SAMPLED_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT |
                  VK_IMAGE_USAGE_TRANSFER_SRC_BIT };
      CHECK(vkCreateImage(dev, &ii, NULL, &im->image));
      VkMemoryRequirements mr;
      vkGetImageMemoryRequirements(dev, im->image, &mr);
      mem_bits &= mr.memoryTypeBits;
      mem_size = (mem_size + mr.alignment - 1) / mr.alignment * mr.alignment;
      im->mem_off = mem_size;
      mem_size += mr.size;
      for (uint32_t m = 0; m < im->mips; m++) {
         uint32_t bw = (mipdim(im->sh->w, m) + im->fmt->bw - 1) / im->fmt->bw;
         uint32_t bh = (mipdim(im->sh->h, m) + im->fmt->bh - 1) / im->fmt->bh;
         im->src_off[m] = staging_size;
         im->src_size[m] = (VkDeviceSize)bw * bh * im->fmt->bytes * im->sh->layers;
         staging_size += (im->src_size[m] + 15) & ~15ull;
         for (int mode = 0; mode < 4; mode++) {
            im->out_base[m][mode] = out_slots;
            out_slots += 1024;
         }
      }
   }
   VkMemoryAllocateInfo mai = { VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = mem_size,
      .memoryTypeIndex = find_mem(mem_bits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT) };
   VkDeviceMemory img_mem;
   CHECK(vkAllocateMemory(dev, &mai, NULL, &img_mem));
   printf("%u images in one %llu KiB memory, %llu KiB of texel data\n", (unsigned)NIMG,
          (unsigned long long)(mem_size >> 10), (unsigned long long)(staging_size >> 10));

   /* staging (source), readback, output: host visible */
   VkBuffer bufs[3];
   void *maps[3];
   VkDeviceSize sizes[3] = { staging_size, staging_size, (VkDeviceSize)out_slots * 16 };
   VkBufferUsageFlags usages[3] = { VK_BUFFER_USAGE_TRANSFER_SRC_BIT, VK_BUFFER_USAGE_TRANSFER_DST_BIT,
                                    VK_BUFFER_USAGE_STORAGE_BUFFER_BIT };
   for (int i = 0; i < 3; i++) {
      VkBufferCreateInfo bci = { VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size = sizes[i], .usage = usages[i] };
      CHECK(vkCreateBuffer(dev, &bci, NULL, &bufs[i]));
      VkMemoryRequirements mr;
      vkGetBufferMemoryRequirements(dev, bufs[i], &mr);
      VkMemoryAllocateInfo bmai = { VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = mr.size,
         .memoryTypeIndex = find_mem(mr.memoryTypeBits, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                                                        VK_MEMORY_PROPERTY_HOST_COHERENT_BIT) };
      VkDeviceMemory m;
      CHECK(vkAllocateMemory(dev, &bmai, NULL, &m));
      CHECK(vkBindBufferMemory(dev, bufs[i], m, 0));
      CHECK(vkMapMemory(dev, m, 0, sizes[i], 0, &maps[i]));
      memset(maps[i], 0xee, sizes[i]);
   }
   uint32_t rng = 0x12345678u;
   for (VkDeviceSize i = 0; i < staging_size; i++) {
      rng ^= rng << 13; rng ^= rng >> 17; rng ^= rng << 5;
      ((uint8_t *)maps[0])[i] = (uint8_t)rng;
   }

   VkSamplerCreateInfo sci = { VK_STRUCTURE_TYPE_SAMPLER_CREATE_INFO, .magFilter = VK_FILTER_NEAREST,
      .minFilter = VK_FILTER_NEAREST, .mipmapMode = VK_SAMPLER_MIPMAP_MODE_NEAREST,
      .addressModeU = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE, .addressModeV = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
      .addressModeW = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE, .maxLod = 16.0f };
   VkSampler smp;
   CHECK(vkCreateSampler(dev, &sci, NULL, &smp));

   uint32_t nsets = 0;
   for (uint32_t i = 0; i < NIMG; i++) {
      struct img *im = &imgs[i];
      CHECK(vkBindImageMemory(dev, im->image, img_mem, im->mem_off));
      VkImageViewCreateInfo vi = { VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO, .image = im->image,
         .viewType = VK_IMAGE_VIEW_TYPE_2D_ARRAY, .format = im->fmt->f,
         .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, im->mips, 0, im->sh->layers } };
      CHECK(vkCreateImageView(dev, &vi, NULL, &im->full));
      for (uint32_t m = 0; m < im->mips; m++) {
         vi.subresourceRange.baseMipLevel = m;
         vi.subresourceRange.levelCount = 1;
         CHECK(vkCreateImageView(dev, &vi, NULL, &im->per_mip[m]));
         nsets += 2;
      }
   }

   VkDescriptorSetLayoutBinding lb[2] = {
      { 0, VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, 1, VK_SHADER_STAGE_COMPUTE_BIT },
      { 1, VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 1, VK_SHADER_STAGE_COMPUTE_BIT },
   };
   VkDescriptorSetLayoutCreateInfo dslci = { VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
      .bindingCount = 2, .pBindings = lb };
   VkDescriptorSetLayout dsl;
   CHECK(vkCreateDescriptorSetLayout(dev, &dslci, NULL, &dsl));
   VkDescriptorPoolSize ps[2] = { { VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, nsets },
                                  { VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, nsets } };
   VkDescriptorPoolCreateInfo dpci = { VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
      .maxSets = nsets, .poolSizeCount = 2, .pPoolSizes = ps };
   VkDescriptorPool dp;
   CHECK(vkCreateDescriptorPool(dev, &dpci, NULL, &dp));
   VkPushConstantRange pcr = { VK_SHADER_STAGE_COMPUTE_BIT, 0, 24 };
   VkPipelineLayoutCreateInfo plci = { VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
      .setLayoutCount = 1, .pSetLayouts = &dsl, .pushConstantRangeCount = 1, .pPushConstantRanges = &pcr };
   VkPipelineLayout pl;
   CHECK(vkCreatePipelineLayout(dev, &plci, NULL, &pl));
   VkShaderModuleCreateInfo smci = { VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
      .codeSize = sizeof(texture_read_comp), .pCode = texture_read_comp };
   VkShaderModule cs;
   CHECK(vkCreateShaderModule(dev, &smci, NULL, &cs));
   VkComputePipelineCreateInfo cpci = { VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO,
      .stage = { VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, .stage = VK_SHADER_STAGE_COMPUTE_BIT,
                 .module = cs, .pName = "main" }, .layout = pl };
   VkPipeline pipe;
   CHECK(vkCreateComputePipelines(dev, VK_NULL_HANDLE, 1, &cpci, NULL, &pipe));

   VkCommandPoolCreateInfo cpi = { VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .queueFamilyIndex = qf };
   VkCommandPool pool;
   CHECK(vkCreateCommandPool(dev, &cpi, NULL, &pool));
   VkCommandBufferAllocateInfo cbai = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
      .commandPool = pool, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1 };
   VkCommandBuffer cb;
   CHECK(vkAllocateCommandBuffers(dev, &cbai, &cb));
   VkCommandBufferBeginInfo cbbi = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
      .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT };
   CHECK(vkBeginCommandBuffer(cb, &cbbi));

   for (uint32_t i = 0; i < NIMG; i++) {
      struct img *im = &imgs[i];
      VkImageMemoryBarrier b = { VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
         .dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT, .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
         .newLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
         .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .image = im->image,
         .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, im->mips, 0, im->sh->layers } };
      vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT,
                           0, 0, NULL, 0, NULL, 1, &b);
      for (uint32_t m = 0; m < im->mips; m++) {
         VkBufferImageCopy c = { .bufferOffset = im->src_off[m],
            .imageSubresource = { VK_IMAGE_ASPECT_COLOR_BIT, m, 0, im->sh->layers },
            .imageExtent = { mipdim(im->sh->w, m), mipdim(im->sh->h, m), 1 } };
         vkCmdCopyBufferToImage(cb, bufs[0], im->image, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, 1, &c);
      }
      b.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
      b.dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT | VK_ACCESS_SHADER_READ_BIT;
      b.oldLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL;
      b.newLayout = VK_IMAGE_LAYOUT_GENERAL;
      vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TRANSFER_BIT,
                           VK_PIPELINE_STAGE_TRANSFER_BIT | VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
                           0, 0, NULL, 0, NULL, 1, &b);
      for (uint32_t m = 0; m < im->mips; m++) {
         VkBufferImageCopy c = { .bufferOffset = im->src_off[m],
            .imageSubresource = { VK_IMAGE_ASPECT_COLOR_BIT, m, 0, im->sh->layers },
            .imageExtent = { mipdim(im->sh->w, m), mipdim(im->sh->h, m), 1 } };
         vkCmdCopyImageToBuffer(cb, im->image, VK_IMAGE_LAYOUT_GENERAL, bufs[1], 1, &c);
      }
   }

   vkCmdBindPipeline(cb, VK_PIPELINE_BIND_POINT_COMPUTE, pipe);
   for (uint32_t i = 0; i < NIMG; i++) {
      struct img *im = &imgs[i];
      for (uint32_t m = 0; m < im->mips; m++) {
         for (int v = 0; v < 2; v++) {
            VkDescriptorSetAllocateInfo dsai = { VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
               .descriptorPool = dp, .descriptorSetCount = 1, .pSetLayouts = &dsl };
            VkDescriptorSet ds;
            CHECK(vkAllocateDescriptorSets(dev, &dsai, &ds));
            VkDescriptorImageInfo dii = { smp, v ? im->per_mip[m] : im->full, VK_IMAGE_LAYOUT_GENERAL };
            VkDescriptorBufferInfo dbi = { bufs[2], 0, VK_WHOLE_SIZE };
            VkWriteDescriptorSet w[2] = {
               { VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = ds, .dstBinding = 0, .descriptorCount = 1,
                 .descriptorType = VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, .pImageInfo = &dii },
               { VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = ds, .dstBinding = 1, .descriptorCount = 1,
                 .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, .pBufferInfo = &dbi },
            };
            vkUpdateDescriptorSets(dev, 2, w, 0, NULL);
            vkCmdBindDescriptorSets(cb, VK_PIPELINE_BIND_POINT_COMPUTE, pl, 0, 1, &ds, 0, NULL);
            for (int mode = 0; mode < 2; mode++) {
               uint32_t layer = im->sh->layers - 1;
               uint32_t pc[6] = { im->out_base[m][v * 2 + mode], v ? 0 : m, mipdim(im->sh->w, m),
                                  mipdim(im->sh->h, m), layer, (uint32_t)mode };
               vkCmdPushConstants(cb, pl, VK_SHADER_STAGE_COMPUTE_BIT, 0, 24, pc);
               vkCmdDispatch(cb, 4, 4, 1);
            }
         }
      }
   }
   VkMemoryBarrier host = { VK_STRUCTURE_TYPE_MEMORY_BARRIER,
      .srcAccessMask = VK_ACCESS_SHADER_WRITE_BIT | VK_ACCESS_TRANSFER_WRITE_BIT,
      .dstAccessMask = VK_ACCESS_HOST_READ_BIT };
   vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT, VK_PIPELINE_STAGE_HOST_BIT,
                        0, 1, &host, 0, NULL, 0, NULL);
   CHECK(vkEndCommandBuffer(cb));
   VkFenceCreateInfo fci = { VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
   VkFence fence;
   CHECK(vkCreateFence(dev, &fci, NULL, &fence));
   VkSubmitInfo si = { VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cb };
   CHECK(vkQueueSubmit(q, 1, &si, fence));
   CHECK(vkWaitForFences(dev, 1, &fence, VK_TRUE, 10ull * 1000 * 1000 * 1000));

   uint32_t bad = 0;
   uint64_t all = 0xcbf29ce484222325ull;
   for (uint32_t i = 0; i < NIMG; i++) {
      struct img *im = &imgs[i];
      uint32_t copy_bad = 0, view_bad = 0, first_copy_bad_mip = ~0u, first_view_bad_mip = ~0u;
      uint64_t h = 0xcbf29ce484222325ull;
      for (uint32_t m = 0; m < im->mips; m++) {
         const uint8_t *src = (const uint8_t *)maps[0] + im->src_off[m];
         const uint8_t *dst = (const uint8_t *)maps[1] + im->src_off[m];
         if (memcmp(src, dst, im->src_size[m])) {
            copy_bad++;
            if (first_copy_bad_mip == ~0u) first_copy_bad_mip = m;
         }
         const float *o0 = (const float *)maps[2] + (size_t)im->out_base[m][0] * 4;
         uint32_t gw = mipdim(im->sh->w, m) < 32 ? mipdim(im->sh->w, m) : 32;
         uint32_t gh = mipdim(im->sh->h, m) < 32 ? mipdim(im->sh->h, m) : 32;
         for (int mode = 1; mode < 4; mode++) {
            const float *on = (const float *)maps[2] + (size_t)im->out_base[m][mode] * 4;
            for (uint32_t y = 0; y < gh; y++)
               if (memcmp(o0 + y * 32 * 4, on + y * 32 * 4, gw * 16)) {
                  view_bad++;
                  if (first_view_bad_mip == ~0u) first_view_bad_mip = m;
                  break;
               }
         }
         for (uint32_t y = 0; y < gh; y++)
            h = fnv(h, o0 + y * 32 * 4, gw * 16);
      }
      all = fnv(all, &h, 8);
      int ok = !copy_bad && !view_bad;
      if (!ok) bad++;
      printf("%-5s %4ux%-4u x%u mips %2u: %s copy_bad_mips=%u (first %d) view_mismatch=%u (first mip %d) hash %016llx\n",
             im->fmt->name, im->sh->w, im->sh->h, im->sh->layers, im->mips, ok ? "ok  " : "FAIL",
             copy_bad, (int)first_copy_bad_mip, view_bad, (int)first_view_bad_mip, (unsigned long long)h);
   }
   printf("RESULT %s bad_images=%u all_hash=%016llx\n", bad ? "FAIL" : "PASS", bad,
          (unsigned long long)all);
   vkDeviceWaitIdle(dev);
   vkDestroyDevice(dev, NULL);
   vkDestroyInstance(inst, NULL);
   return bad ? 2 : 0;
}
