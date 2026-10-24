/* scale_eval: the composed present's scalers against a known truth, on any
 * Vulkan GPU (Linux host) (docs/enhance.md): a scene rendered at the output
 * size is the truth, the same scene box-filtered to the input size goes
 * through each scaler, PSNR against the truth.
 */
#include <vulkan/vulkan.h>
#include <math.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define ARRAY_LEN(a) (sizeof(a) / sizeof((a)[0]))

#define CK(x) do { VkResult r_ = (x); if (r_ != VK_SUCCESS) { \
   fprintf(stderr, "%s:%d %s = %d\n", __FILE__, __LINE__, #x, r_); exit(1); } } while (0)

static VkDevice dev;
static VkPhysicalDevice pd;
static VkQueue queue;
static VkCommandPool pool;
static PFN_vkCmdPushDescriptorSetKHR push_desc;

struct img { VkImage i; VkDeviceMemory m; VkImageView v; uint32_t w, h; };

static uint32_t
memtype(uint32_t bits, VkMemoryPropertyFlags want)
{
   VkPhysicalDeviceMemoryProperties mp;
   vkGetPhysicalDeviceMemoryProperties(pd, &mp);
   for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
      if ((bits & (1u << i)) && (mp.memoryTypes[i].propertyFlags & want) == want)
         return i;
   exit(2);
}

static struct img
mkimg(VkFormat f, uint32_t w, uint32_t h)
{
   struct img o = { .w = w, .h = h };
   VkImageCreateInfo ci = { VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO, .imageType = VK_IMAGE_TYPE_2D,
      .format = f, .extent = { w, h, 1 }, .mipLevels = 1, .arrayLayers = 1,
      .samples = VK_SAMPLE_COUNT_1_BIT, .tiling = VK_IMAGE_TILING_OPTIMAL,
      .usage = VK_IMAGE_USAGE_STORAGE_BIT | VK_IMAGE_USAGE_SAMPLED_BIT |
               VK_IMAGE_USAGE_TRANSFER_DST_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT };
   CK(vkCreateImage(dev, &ci, NULL, &o.i));
   VkMemoryRequirements mr;
   vkGetImageMemoryRequirements(dev, o.i, &mr);
   VkMemoryAllocateInfo ai = { VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = mr.size,
      .memoryTypeIndex = memtype(mr.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT) };
   CK(vkAllocateMemory(dev, &ai, NULL, &o.m));
   CK(vkBindImageMemory(dev, o.i, o.m, 0));
   VkImageViewCreateInfo vi = { VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO, .image = o.i,
      .viewType = VK_IMAGE_VIEW_TYPE_2D, .format = f,
      .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
   CK(vkCreateImageView(dev, &vi, NULL, &o.v));
   return o;
}

struct buf { VkBuffer b; VkDeviceMemory m; void *p; };
static struct buf
mkbuf(VkDeviceSize size)
{
   struct buf o;
   VkBufferCreateInfo ci = { VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size = size,
      .usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT | VK_BUFFER_USAGE_TRANSFER_DST_BIT };
   CK(vkCreateBuffer(dev, &ci, NULL, &o.b));
   VkMemoryRequirements mr;
   vkGetBufferMemoryRequirements(dev, o.b, &mr);
   VkMemoryAllocateInfo ai = { VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = mr.size,
      .memoryTypeIndex = memtype(mr.memoryTypeBits, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                                                    VK_MEMORY_PROPERTY_HOST_COHERENT_BIT) };
   CK(vkAllocateMemory(dev, &ai, NULL, &o.m));
   CK(vkBindBufferMemory(dev, o.b, o.m, 0));
   CK(vkMapMemory(dev, o.m, 0, size, 0, &o.p));
   return o;
}

struct push { int32_t src[2], dst[2], off[2], aux[2]; float p0[4], p1[4]; uint32_t mode, flags; };

static VkPipelineLayout layout;
static VkSampler lin, nearest;

static VkPipeline
pipeline(const char *dir, const char *name)
{
   char path[512];
   snprintf(path, sizeof(path), "%s/%s.spv", dir, name);
   FILE *f = fopen(path, "rb");
   if (!f) { fprintf(stderr, "no %s\n", path); exit(1); }
   fseek(f, 0, SEEK_END);
   long n = ftell(f);
   fseek(f, 0, SEEK_SET);
   uint32_t *code = malloc(n);
   if (fread(code, 1, n, f) != (size_t)n) exit(1);
   fclose(f);
   VkShaderModuleCreateInfo sci = { VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
                                    .codeSize = n, .pCode = code };
   VkShaderModule m;
   CK(vkCreateShaderModule(dev, &sci, NULL, &m));
   VkComputePipelineCreateInfo pci = { VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO,
      .stage = { VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
                 .stage = VK_SHADER_STAGE_COMPUTE_BIT, .module = m, .pName = "main" },
      .layout = layout };
   VkPipeline p;
   CK(vkCreateComputePipelines(dev, VK_NULL_HANDLE, 1, &pci, NULL, &p));
   free(code);
   return p;
}

static void
barrier(VkCommandBuffer cb)
{
   VkMemoryBarrier mb = { VK_STRUCTURE_TYPE_MEMORY_BARRIER,
      .srcAccessMask = VK_ACCESS_SHADER_WRITE_BIT | VK_ACCESS_TRANSFER_WRITE_BIT,
      .dstAccessMask = VK_ACCESS_SHADER_READ_BIT | VK_ACCESS_SHADER_WRITE_BIT |
                       VK_ACCESS_TRANSFER_READ_BIT };
   vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT,
                        VK_PIPELINE_STAGE_ALL_COMMANDS_BIT, 0, 1, &mb, 0, NULL, 0, NULL);
}

static void
dispatch(VkCommandBuffer cb, VkPipeline p, VkImageView s0, VkImageView s1, VkImageView s2,
         VkImageView dst, const struct push *pc)
{
   VkDescriptorImageInfo ii[4] = {
      { lin, s0, VK_IMAGE_LAYOUT_GENERAL }, { lin, s1 ? s1 : s0, VK_IMAGE_LAYOUT_GENERAL },
      { nearest, s2 ? s2 : s0, VK_IMAGE_LAYOUT_GENERAL }, { VK_NULL_HANDLE, dst, VK_IMAGE_LAYOUT_GENERAL } };
   VkWriteDescriptorSet w[4];
   for (int i = 0; i < 4; i++)
      w[i] = (VkWriteDescriptorSet){ VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstBinding = i,
         .descriptorCount = 1, .descriptorType = i < 3 ? VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER
                                                       : VK_DESCRIPTOR_TYPE_STORAGE_IMAGE,
         .pImageInfo = &ii[i] };
   vkCmdBindPipeline(cb, VK_PIPELINE_BIND_POINT_COMPUTE, p);
   push_desc(cb, VK_PIPELINE_BIND_POINT_COMPUTE, layout, 0, 4, w);
   vkCmdPushConstants(cb, layout, VK_SHADER_STAGE_COMPUTE_BIT, 0, sizeof(*pc), pc);
   vkCmdDispatch(cb, (pc->dst[0] + 7) / 8, (pc->dst[1] + 7) / 8, 1);
}


static float
hash2(int x, int y)
{
   uint32_t h = (uint32_t)x * 374761393u + (uint32_t)y * 668265263u;
   h = (h ^ (h >> 13)) * 1274126177u;
   return (float)((h ^ (h >> 16)) & 0xffff) / 65535.0f;
}

static float
noise(float x, float y)
{
   int xi = (int)floorf(x), yi = (int)floorf(y);
   float fx = x - xi, fy = y - yi;
   fx = fx * fx * (3 - 2 * fx);
   fy = fy * fy * (3 - 2 * fy);
   float a = hash2(xi, yi), b = hash2(xi + 1, yi), c = hash2(xi, yi + 1), d = hash2(xi + 1, yi + 1);
   return (a + (b - a) * fx) * (1 - fy) + (c + (d - c) * fx) * fy;
}

/* scene at normalised (u, v): 0 photo-like (texture, diagonal edges, thin
 * lines, text strokes), 1 pixel art (a 320x180 sprite sheet, nearest) */
static void
scene(int s, float u, float v, float rgb[3])
{
   if (s == 1) {
      const int px = (int)(u * 320), py = (int)(v * 180);
      const int tile = (px / 16 + py / 16 * 7) % 5;
      const int lx = px % 16, ly = py % 16;
      const bool body = (lx - 8) * (lx - 8) + (ly - 8) * (ly - 8) < 30 + tile * 6;
      const bool outline = !body && (lx - 8) * (lx - 8) + (ly - 8) * (ly - 8) < 48 + tile * 6;
      const float pal[5][3] = { { 0.9f, 0.2f, 0.2f }, { 0.2f, 0.8f, 0.3f }, { 0.2f, 0.4f, 0.9f },
                                { 0.9f, 0.8f, 0.2f }, { 0.8f, 0.3f, 0.8f } };
      if (body) { for (int k = 0; k < 3; k++) rgb[k] = pal[tile][k]; }
      else if (outline) { rgb[0] = rgb[1] = rgb[2] = 0.05f; }
      else { rgb[0] = 0.15f; rgb[1] = 0.2f; rgb[2] = 0.3f + 0.1f * ((px + py) % 2); }
      return;
   }
   const float x = u * 2560, y = v * 1440;
   float n = 0.6f * noise(x / 23, y / 23) + 0.4f * noise(x / 7, y / 7);
   rgb[0] = 0.2f + 0.6f * n; rgb[1] = 0.25f + 0.5f * n; rgb[2] = 0.3f + 0.4f * n;
   /* diagonal edges */
   if (fmodf(x * 0.7f + y, 260.0f) < 90.0f && x > 1200) { rgb[0] = 0.95f; rgb[1] = 0.6f; rgb[2] = 0.1f; }
   /* thin lines */
   if (y > 900 && y < 1300 && x < 1100 && fmodf(x + y * 0.25f, 18.0f) < 2.0f) rgb[0] = rgb[1] = rgb[2] = 0.95f;
   /* text strokes */
   if (y > 120 && y < 520 && x > 120 && x < 1000) {
      const int line = (int)((y - 120) / 40), row = (int)(y - 120) % 40, col = (int)(x - 120) / 14, cx = (int)(x - 120) % 14;
      if (row > 6 && row < 30 && cx < 9 && hash2(col, line) > 0.3f &&
          (cx < 2 || row < 9 || (row > 16 && row < 19) || hash2(col * 3 + cx, line + row / 6) > 0.6f))
         rgb[0] = rgb[1] = rgb[2] = 0.05f;
   }
}

static void
render(int s, int w, int h, int ss, uint8_t *px)
{
   for (int y = 0; y < h; y++)
      for (int x = 0; x < w; x++) {
         float acc[3] = { 0, 0, 0 };
         for (int j = 0; j < ss; j++)
            for (int i = 0; i < ss; i++) {
               float c[3];
               scene(s, (x + (i + 0.5f) / ss) / w, (y + (j + 0.5f) / ss) / h, c);
               for (int k = 0; k < 3; k++) acc[k] += c[k];
            }
         uint8_t *p = px + 4 * (y * w + x);
         for (int k = 0; k < 3; k++)
            p[k] = (uint8_t)fminf(255.0f, acc[k] / (ss * ss) * 255.0f + 0.5f);
         p[3] = 255;
      }
}

struct pnis {
   float f[18];
   uint32_t u[8];
};

int
main(int argc, char **argv)
{
   if (argc < 2) { fprintf(stderr, "usage: scale_eval SPV_DIR [OUT_DIR]\n"); return 2; }
   const char *spv = argv[1], *out_dir = argc > 2 ? argv[2] : NULL;
   VkApplicationInfo app = { VK_STRUCTURE_TYPE_APPLICATION_INFO, .apiVersion = VK_API_VERSION_1_3 };
   VkInstanceCreateInfo ici = { VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app };
   VkInstance inst;
   CK(vkCreateInstance(&ici, NULL, &inst));
   uint32_t n = 8;
   VkPhysicalDevice pds[8];
   CK(vkEnumeratePhysicalDevices(inst, &n, pds));
   pd = pds[0];
   for (uint32_t i = 0; i < n; i++) {
      VkPhysicalDeviceProperties p;
      vkGetPhysicalDeviceProperties(pds[i], &p);
      if (p.deviceType == VK_PHYSICAL_DEVICE_TYPE_DISCRETE_GPU) { pd = pds[i]; break; }
   }
   VkPhysicalDeviceProperties props;
   vkGetPhysicalDeviceProperties(pd, &props);
   float prio = 1;
   VkDeviceQueueCreateInfo qci = { VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueFamilyIndex = 0,
                                   .queueCount = 1, .pQueuePriorities = &prio };
   const char *ext[] = { VK_KHR_PUSH_DESCRIPTOR_EXTENSION_NAME };
   VkDeviceCreateInfo dci = { VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .queueCreateInfoCount = 1,
      .pQueueCreateInfos = &qci, .enabledExtensionCount = 1, .ppEnabledExtensionNames = ext };
   CK(vkCreateDevice(pd, &dci, NULL, &dev));
   vkGetDeviceQueue(dev, 0, 0, &queue);
   push_desc = (PFN_vkCmdPushDescriptorSetKHR)vkGetDeviceProcAddr(dev, "vkCmdPushDescriptorSetKHR");
   VkSamplerCreateInfo sci = { VK_STRUCTURE_TYPE_SAMPLER_CREATE_INFO, .magFilter = VK_FILTER_LINEAR,
      .minFilter = VK_FILTER_LINEAR, .addressModeU = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
      .addressModeV = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE, .addressModeW = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE };
   CK(vkCreateSampler(dev, &sci, NULL, &lin));
   sci.magFilter = sci.minFilter = VK_FILTER_NEAREST;
   CK(vkCreateSampler(dev, &sci, NULL, &nearest));
   VkDescriptorSetLayoutBinding b[4];
   for (int i = 0; i < 4; i++)
      b[i] = (VkDescriptorSetLayoutBinding){ i, i < 3 ? VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER
                                                     : VK_DESCRIPTOR_TYPE_STORAGE_IMAGE, 1, VK_SHADER_STAGE_COMPUTE_BIT };
   VkDescriptorSetLayoutCreateInfo dl = { VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
      .flags = VK_DESCRIPTOR_SET_LAYOUT_CREATE_PUSH_DESCRIPTOR_BIT_KHR, .bindingCount = 4, .pBindings = b };
   VkDescriptorSetLayout dsl;
   CK(vkCreateDescriptorSetLayout(dev, &dl, NULL, &dsl));
   VkPushConstantRange pcr = { VK_SHADER_STAGE_COMPUTE_BIT, 0, 128 };
   VkPipelineLayoutCreateInfo lci = { VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
      .setLayoutCount = 1, .pSetLayouts = &dsl, .pushConstantRangeCount = 1, .pPushConstantRanges = &pcr };
   CK(vkCreatePipelineLayout(dev, &lci, NULL, &layout));
   enum { NEAREST, BILINEAR, BICUBIC, FSR_EASU, FSR, NIS, CAS, SGSR, XBR, NMODES };
   const char *mname[NMODES] = { "nearest", "bilinear", "bicubic", "fsr (EASU)", "fsr (+RCAS)",
                                 "nis", "cas", "sgsr", "xbr" };
   VkPipeline ps = pipeline(spv, "enh_scale"), pe = pipeline(spv, "enh_easu"),
              pr = pipeline(spv, "enh_rcas"), pn = pipeline(spv, "enh_nis"),
              pc_ = pipeline(spv, "enh_cas"), pg = pipeline(spv, "enh_sgsr"), px = pipeline(spv, "enh_xbr");
   VkCommandPoolCreateInfo cpi = { VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
                                   .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT };
   CK(vkCreateCommandPool(dev, &cpi, NULL, &pool));
   VkCommandBuffer cb;
   VkCommandBufferAllocateInfo cai = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
                                       .commandPool = pool, .commandBufferCount = 1 };
   CK(vkAllocateCommandBuffers(dev, &cai, &cb));
   VkQueryPoolCreateInfo qpi = { VK_STRUCTURE_TYPE_QUERY_POOL_CREATE_INFO,
                                 .queryType = VK_QUERY_TYPE_TIMESTAMP, .queryCount = 2 };
   VkQueryPool qp;
   CK(vkCreateQueryPool(dev, &qpi, NULL, &qp));

   const int OW = 2560, OH = 1440;
   const struct { int s, w, h; const char *name; } cases[] = {
      { 0, 1280, 720, "photo 1280x720 -> 2560x1440" },
      { 0, 1920, 1080, "photo 1920x1080 -> 2560x1440" },
      { 1, 640, 360, "pixel art 640x360 -> 2560x1440" },
   };
   struct img out = mkimg(VK_FORMAT_R8G8B8A8_UNORM, OW, OH), tmp = mkimg(VK_FORMAT_R8G8B8A8_UNORM, OW, OH);
   struct buf up = mkbuf((VkDeviceSize)OW * OH * 4), down = mkbuf((VkDeviceSize)OW * OH * 4);
   uint8_t *truth = malloc(OW * OH * 4);
   for (unsigned ci = 0; ci < ARRAY_LEN(cases); ci++) {
      const int W_ = cases[ci].w, H_ = cases[ci].h;
      struct img in = mkimg(VK_FORMAT_R8G8B8A8_UNORM, W_, H_);
      render(cases[ci].s, OW, OH, cases[ci].s == 1 ? 1 : 2, truth);
      render(cases[ci].s, W_, H_, cases[ci].s == 1 ? 1 : 4, up.p);
      printf("== %s\n%-12s %8s %8s\n", cases[ci].name, "scaler", "dB", "us");
      for (int m = 0; m < NMODES; m++) {
         VkCommandBufferBeginInfo bi = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO };
         CK(vkBeginCommandBuffer(cb, &bi));
         VkImageMemoryBarrier ib[3];
         struct img *all[3] = { &in, &out, &tmp };
         for (int i = 0; i < 3; i++)
            ib[i] = (VkImageMemoryBarrier){ VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
               .dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT | VK_ACCESS_SHADER_WRITE_BIT,
               .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED, .newLayout = VK_IMAGE_LAYOUT_GENERAL,
               .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
               .image = all[i]->i, .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
         vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT,
                              0, 0, NULL, 0, NULL, 3, ib);
         VkBufferImageCopy c = { .imageSubresource = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1 },
                                 .imageExtent = { W_, H_, 1 } };
         vkCmdCopyBufferToImage(cb, up.b, in.i, VK_IMAGE_LAYOUT_GENERAL, 1, &c);
         barrier(cb);
         vkCmdResetQueryPool(cb, qp, 0, 2);
         vkCmdWriteTimestamp(cb, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, qp, 0);
         struct push p;
         memset(&p, 0, sizeof(p));
         p.src[0] = W_; p.src[1] = H_; p.dst[0] = OW; p.dst[1] = OH;
         switch (m) {
         case NEAREST: case BILINEAR: case BICUBIC:
            p.mode = m == NEAREST ? 0 : m == BILINEAR ? 1 : 2;
            dispatch(cb, ps, in.v, NULL, NULL, out.v, &p);
            break;
         case FSR_EASU:
            dispatch(cb, pe, in.v, NULL, NULL, out.v, &p);
            break;
         case FSR:
            dispatch(cb, pe, in.v, NULL, NULL, tmp.v, &p);
            barrier(cb);
            p.src[0] = OW; p.src[1] = OH;
            p.p0[0] = 1.0f;  /* sharpness 50 */
            dispatch(cb, pr, tmp.v, NULL, NULL, out.v, &p);
            break;
         case CAS:
            p.p0[0] = 0.5f;
            dispatch(cb, pc_, in.v, NULL, NULL, out.v, &p);
            break;
         case SGSR:
            dispatch(cb, pg, in.v, NULL, NULL, out.v, &p);
            break;
         case XBR: {
            /* nearest sampler for xBR */
            VkSampler keep = lin;
            lin = nearest;
            dispatch(cb, px, in.v, NULL, NULL, out.v, &p);
            lin = keep;
            break;
         }
         case NIS: {
            if (OW > 2 * W_) {
               /* outside NIS's 1x..2x: not run (the driver uses FSR) */
               break;
            }
            struct pnis q;
            memset(&q, 0, sizeof(q));
            const float s = 0.5f - 0.5f;
            const float mx = s >= 0 ? 1.25f : 1.75f, mn = s >= 0 ? 1.25f : 1.0f, ls = s >= 0 ? 1.25f : 1.0f;
            const float smin = fmaxf(0.0f, 0.4f + s * mn * 1.2f), smax = 1.6f + s * mx * 1.8f;
            const float lmin = fmaxf(0.1f, 0.14f + s * ls * 0.32f), lmax = 0.5f + s * ls * 0.6f;
            q.f[0] = 2 * 1127.f / 1024.f; q.f[1] = 64.0f / 1024.0f; q.f[2] = 2.0f; q.f[3] = 1.0f / 8.0f;
            q.f[4] = 1.0f; q.f[5] = 1.0f / 255.0f; q.f[6] = 0.45f; q.f[7] = 1.0f / (0.9f - 0.45f);
            q.f[8] = smin; q.f[9] = smax - smin; q.f[10] = lmin; q.f[11] = lmax - lmin;
            q.f[12] = (float)W_ / OW; q.f[13] = (float)H_ / OH; q.f[14] = 1.0f / OW; q.f[15] = 1.0f / OH;
            q.f[16] = 1.0f / W_; q.f[17] = 1.0f / H_;
            q.u[2] = W_; q.u[3] = H_; q.u[6] = OW; q.u[7] = OH;
            VkDescriptorImageInfo ii[4] = { { lin, in.v, VK_IMAGE_LAYOUT_GENERAL }, { lin, in.v, VK_IMAGE_LAYOUT_GENERAL },
               { nearest, in.v, VK_IMAGE_LAYOUT_GENERAL }, { VK_NULL_HANDLE, out.v, VK_IMAGE_LAYOUT_GENERAL } };
            VkWriteDescriptorSet w[4];
            for (int i = 0; i < 4; i++)
               w[i] = (VkWriteDescriptorSet){ VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstBinding = i,
                  .descriptorCount = 1, .descriptorType = i < 3 ? VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER
                                                                : VK_DESCRIPTOR_TYPE_STORAGE_IMAGE, .pImageInfo = &ii[i] };
            vkCmdBindPipeline(cb, VK_PIPELINE_BIND_POINT_COMPUTE, pn);
            push_desc(cb, VK_PIPELINE_BIND_POINT_COMPUTE, layout, 0, 4, w);
            vkCmdPushConstants(cb, layout, VK_SHADER_STAGE_COMPUTE_BIT, 0, sizeof(q), &q);
            vkCmdDispatch(cb, (OW + 31) / 32, (OH + 23) / 24, 1);
            break;
         }
         }
         vkCmdWriteTimestamp(cb, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, qp, 1);
         barrier(cb);
         VkBufferImageCopy c2 = { .imageSubresource = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1 },
                                  .imageExtent = { OW, OH, 1 } };
         vkCmdCopyImageToBuffer(cb, out.i, VK_IMAGE_LAYOUT_GENERAL, down.b, 1, &c2);
         CK(vkEndCommandBuffer(cb));
         VkSubmitInfo si = { VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cb };
         CK(vkQueueSubmit(queue, 1, &si, VK_NULL_HANDLE));
         CK(vkQueueWaitIdle(queue));
         if (m == NIS && OW > 2 * W_) {
            printf("%-12s %8s\n", mname[m], "(>2x)");
            continue;
         }
         uint64_t ts[2];
         vkGetQueryPoolResults(dev, qp, 0, 2, sizeof(ts), ts, 8, VK_QUERY_RESULT_64_BIT);
         const uint8_t *g = down.p;
         double se = 0;
         for (int i = 0; i < OW * OH; i++)
            for (int k = 0; k < 3; k++) {
               const double d = (double)g[4 * i + k] - truth[4 * i + k];
               se += d * d;
            }
         const double mse = se / (OW * OH * 3.0);
         printf("%-12s %8.2f %8.0f\n", mname[m], 10.0 * log10(255.0 * 255.0 / (mse > 1e-9 ? mse : 1e-9)),
                (ts[1] - ts[0]) * props.limits.timestampPeriod / 1000.0);
         if (out_dir) {
            char path[512];
            snprintf(path, sizeof(path), "%s/case%u-%d.ppm", out_dir, ci, m);
            FILE *f = fopen(path, "wb");
            if (f) {
               fprintf(f, "P6\n%d %d\n255\n", OW, OH);
               for (int i = 0; i < OW * OH; i++) fwrite(g + 4 * i, 1, 3, f);
               fclose(f);
            }
         }
      }
   }
   return 0;
}
