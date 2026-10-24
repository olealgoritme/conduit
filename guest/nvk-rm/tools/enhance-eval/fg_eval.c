/* fg_eval: the composed present's frame-generation passes against synthetic
 * scenes with a known truth, on any Vulkan GPU (Linux host), so the flow and
 * interpolation can be judged without a guest (docs/enhance.md).
 *
 *   fg_eval SPV_DIR [fast|quality] [OUT_DIR]
 *
 * SPV_DIR holds fg_down.spv, fg_flow.spv and fg_interp.spv compiled from
 * src/vulkan/wsi/enhance (run.sh does it). For each scene, frames at t=0 and
 * t=1 go through the same dispatch sequence as wsi_common_win32_enhance.c
 * (luma pyramid, flow coarse to fine, interpolation at t=0.5) and the result
 * is compared with the scene rendered at t=0.5:
 *
 *   all    PSNR over the frame
 *   edge   mean absolute error (0..255) within 12 pixels of a moving edge
 *   hud    mean absolute error over the static HUD
 *
 * With OUT_DIR, <scene>-{gen,truth,diff}.ppm are written.
 */
#include <vulkan/vulkan.h>
#include <math.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define W 1280
#define H 720
#define LEVELS 6

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

/* ---- scenes ---------------------------------------------------------------- */

static uint16_t
f2h(float f)
{
   /* float to half, round toward zero; values here are small */
   uint32_t u;
   memcpy(&u, &f, 4);
   const uint32_t sign = (u >> 16) & 0x8000u;
   const int32_t e = (int32_t)((u >> 23) & 0xff) - 127 + 15;
   if (e <= 0)
      return (uint16_t)sign;
   if (e >= 31)
      return (uint16_t)(sign | 0x7c00u);
   return (uint16_t)(sign | ((uint32_t)e << 10) | ((u >> 13) & 0x3ffu));
}

static float
h2f(uint16_t h)
{
   const uint32_t sign = (uint32_t)(h & 0x8000u) << 16;
   const uint32_t e = (h >> 10) & 0x1f, m = h & 0x3ffu;
   uint32_t u;
   if (e == 0)
      u = sign; /* zero and denormals */
   else
      u = sign | ((e - 15 + 127) << 23) | (m << 13);
   float f;
   memcpy(&f, &u, 4);
   return f;
}

static float
hash2(int x, int y)
{
   uint32_t h = (uint32_t)x * 374761393u + (uint32_t)y * 668265263u;
   h = (h ^ (h >> 13)) * 1274126177u;
   return (float)((h ^ (h >> 16)) & 0xffff) / 65535.0f;
}

/* smooth value noise, period-free */
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

static void
texel(float x, float y, float scale, float tint, float rgb[3])
{
   float n = 0.6f * noise(x / scale, y / scale) + 0.3f * noise(x / (scale / 3), y / (scale / 3)) +
             0.1f * hash2((int)x, (int)y);
   rgb[0] = n * (0.4f + tint);
   rgb[1] = n * 0.7f;
   rgb[2] = n * (1.0f - tint);
}

enum { SC_BOX_GRID, SC_BOX_TEXTURE, SC_PAN_HUD, SC_CROSSING, SC_FAST, SC_TEXT, SC_MULTI, SC_COUNT };
static const char *scene_name[SC_COUNT] = { "box_grid", "box_texture", "pan_hud", "crossing", "fast",
                                            "scroll_text", "multi" };

/* SC_MULTI: eight small objects, each its own motion */
static const float multi[8][4] = {
   { 100, 100, 20, 6 }, { 300, 420, -14, -10 }, { 520, 140, 8, 22 }, { 700, 500, -26, 0 },
   { 860, 200, 16, -16 }, { 1000, 420, -6, 12 }, { 1120, 120, -18, 4 }, { 600, 320, 30, -8 },
};

/* What is at (x, y) at time t: colour, whether it is HUD, and the moving
 * edges' positions (for the edge band) */
static void
scene(int s, float t, int x, int y, float rgb[3], int *hud)
{
   *hud = 0;
   const bool in_hud = x >= 24 && x < 344 && y >= 24 && y < 88;
   /* background */
   float pan = s == SC_PAN_HUD ? 24.0f * t : 0.0f;
   if (s == SC_BOX_GRID) {
      rgb[0] = 0.05f; rgb[1] = 0.08f; rgb[2] = 0.2f;
      if (x % 32 == 0 || y % 32 == 0) rgb[0] = rgb[1] = rgb[2] = 0.5f;
   } else if (s == SC_TEXT) {
      /* lines of "text" scrolling up 18 px per frame over paper */
      const float yy = y + 18.0f * t;
      const int line = (int)floorf(yy / 22.0f), row = (int)yy - line * 22;
      const int col = x / 9, cx = x % 9;
      const bool glyph = row > 4 && row < 18 && cx < 6 && hash2(col, line) > 0.25f &&
                         ((cx == 0 || cx == 5) ? hash2(col * 7, line + row) > 0.3f : hash2(col + cx, line * 3 + row / 4) > 0.55f);
      rgb[0] = rgb[1] = rgb[2] = glyph ? 0.1f : 0.92f;
   } else {
      texel(x + pan, y, 40.0f, 0.3f, rgb);
   }
   /* moving objects */
   struct { float x0, y0, dx, dy, size; float tint; } obj[2];
   int n = 0;
   switch (s) {
   case SC_BOX_GRID: obj[n++] = (typeof(obj[0])){ 400, 200, 40, 0, 90, -1 }; break;
   case SC_BOX_TEXTURE: obj[n++] = (typeof(obj[0])){ 400, 250, 32, 8, 160, 0.9f }; break;
   case SC_PAN_HUD: obj[n++] = (typeof(obj[0])){ 700, 300, -20, 0, 120, 0.9f }; break;
   case SC_CROSSING:
      obj[n++] = (typeof(obj[0])){ 500, 250, 36, 0, 140, 0.9f };
      obj[n++] = (typeof(obj[0])){ 660, 300, -36, 0, 140, -0.2f };
      break;
   case SC_FAST: obj[n++] = (typeof(obj[0])){ 300, 250, 110, 0, 120, 0.9f }; break;
   default: break;
   }
   if (s == SC_MULTI) {
      for (int i = 0; i < 8; i++) {
         const float ox = multi[i][0] + multi[i][2] * t, oy = multi[i][1] + multi[i][3] * t;
         if (x >= ox && x < ox + 70 && y >= oy && y < oy + 70) {
            texel(x - ox + 300 * i, y - oy, 9.0f, 0.2f + 0.1f * i, rgb);
            rgb[1] = 0.2f + 0.8f * rgb[1];
         }
      }
   }
   for (int i = 0; i < n; i++) {
      float ox = obj[i].x0 + obj[i].dx * t, oy = obj[i].y0 + obj[i].dy * t;
      if (x >= ox && x < ox + obj[i].size && y >= oy && y < oy + obj[i].size) {
         if (obj[i].tint < -0.5f) {
            rgb[0] = rgb[1] = rgb[2] = 1.0f;
         } else {
            texel(x - ox + 1000 * i, y - oy, 12.0f, obj[i].tint, rgb);
            rgb[0] = 0.3f + 0.7f * rgb[0];
         }
      }
   }
   /* static HUD: a panel with "text" (thin strokes) */
   if (in_hud) {
      *hud = 1;
      const int u = x - 24, v = y - 24;
      const bool stroke = (v > 16 && v < 48) && ((u % 14) < 3 || (v % 16) < 2) && (u / 14) % 3 != 2;
      rgb[0] = stroke ? 1.0f : 0.1f;
      rgb[1] = stroke ? 1.0f : 0.1f;
      rgb[2] = stroke ? 0.3f : 0.15f;
   }
}

/* The true motion (frame 0 -> 1) of what is at (x, y) at t = 0.5 */
static void
true_motion(int s, float x, float y, float *mx, float *my)
{
   *mx = s == SC_PAN_HUD ? -24.0f : 0.0f;
   *my = 0.0f;
   if (x >= 24 && x < 344 && y >= 24 && y < 88) {
      *mx = 0.0f;
      return;
   }
   struct { float x0, y0, dx, dy, size; } obj[2];
   int n = 0;
   switch (s) {
   case SC_BOX_GRID: obj[n++] = (typeof(obj[0])){ 400, 200, 40, 0, 90 }; break;
   case SC_BOX_TEXTURE: obj[n++] = (typeof(obj[0])){ 400, 250, 32, 8, 160 }; break;
   case SC_PAN_HUD: obj[n++] = (typeof(obj[0])){ 700, 300, -20, 0, 120 }; break;
   case SC_CROSSING:
      obj[n++] = (typeof(obj[0])){ 500, 250, 36, 0, 140 };
      obj[n++] = (typeof(obj[0])){ 660, 300, -36, 0, 140 };
      break;
   case SC_FAST: obj[n++] = (typeof(obj[0])){ 300, 250, 110, 0, 120 }; break;
   case SC_TEXT: *my = -18.0f; break;
   case SC_MULTI:
      for (int i = 0; i < 8; i++) {
         const float ox = multi[i][0] + multi[i][2] * 0.5f, oy = multi[i][1] + multi[i][3] * 0.5f;
         if (x >= ox && x < ox + 70 && y >= oy && y < oy + 70) {
            *mx = multi[i][2];
            *my = multi[i][3];
         }
      }
      break;
   default: break;
   }
   for (int i = 0; i < n; i++) {
      float ox = obj[i].x0 + obj[i].dx * 0.5f, oy = obj[i].y0 + obj[i].dy * 0.5f;
      if (x >= ox && x < ox + obj[i].size && y >= oy && y < oy + obj[i].size) {
         *mx = obj[i].dx;
         *my = obj[i].dy;
      }
   }
}

static void
render(int s, float t, uint8_t *px, uint8_t *hudmask)
{
   for (int y = 0; y < H; y++) {
      for (int x = 0; x < W; x++) {
         float c[3];
         int hud;
         scene(s, t, x, y, c, &hud);
         uint8_t *p = px + 4 * (y * W + x);
         for (int k = 0; k < 3; k++)
            p[k] = (uint8_t)fminf(255.0f, fmaxf(0.0f, c[k] * 255.0f + 0.5f));
         p[3] = 255;
         if (hudmask)
            hudmask[y * W + x] = (uint8_t)hud;
      }
   }
}

static void
write_ppm(const char *path, const uint8_t *px)
{
   FILE *f = fopen(path, "wb");
   if (!f) return;
   fprintf(f, "P6\n%d %d\n255\n", W, H);
   for (int i = 0; i < W * H; i++)
      fwrite(px + 4 * i, 1, 3, f);
   fclose(f);
}

int
main(int argc, char **argv)
{
   if (argc < 2) {
      fprintf(stderr, "usage: fg_eval SPV_DIR [fast|quality] [OUT_DIR]\n");
      return 2;
   }
   const char *spv = argv[1];
   const bool hq = argc > 2 && !strcmp(argv[2], "quality");
   const char *out_dir = argc > 3 ? argv[3] : NULL;
   /* FG_INTERP_MODE: fg_interp's pc.mode (default: the driver's choice) */
   const char *im = getenv("FG_INTERP_MODE");
   const uint32_t interp_mode = im ? (uint32_t)atoi(im) : 1u;

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
      .addressModeV = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
      .addressModeW = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE };
   CK(vkCreateSampler(dev, &sci, NULL, &lin));
   sci.magFilter = sci.minFilter = VK_FILTER_NEAREST;
   CK(vkCreateSampler(dev, &sci, NULL, &nearest));
   VkDescriptorSetLayoutBinding b[4];
   for (int i = 0; i < 4; i++)
      b[i] = (VkDescriptorSetLayoutBinding){ i, i < 3 ? VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER
                                                     : VK_DESCRIPTOR_TYPE_STORAGE_IMAGE, 1,
                                              VK_SHADER_STAGE_COMPUTE_BIT };
   VkDescriptorSetLayoutCreateInfo dl = { VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
      .flags = VK_DESCRIPTOR_SET_LAYOUT_CREATE_PUSH_DESCRIPTOR_BIT_KHR, .bindingCount = 4, .pBindings = b };
   VkDescriptorSetLayout dsl;
   CK(vkCreateDescriptorSetLayout(dev, &dl, NULL, &dsl));
   VkPushConstantRange pcr = { VK_SHADER_STAGE_COMPUTE_BIT, 0, sizeof(struct push) };
   VkPipelineLayoutCreateInfo lci = { VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
      .setLayoutCount = 1, .pSetLayouts = &dsl, .pushConstantRangeCount = 1, .pPushConstantRanges = &pcr };
   CK(vkCreatePipelineLayout(dev, &lci, NULL, &layout));
   VkPipeline p_down = pipeline(spv, "fg_down"), p_flow = pipeline(spv, "fg_flow"),
              p_interp = pipeline(spv, "fg_interp");

   VkCommandPoolCreateInfo cpi = { VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
                                   .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT };
   CK(vkCreateCommandPool(dev, &cpi, NULL, &pool));
   VkCommandBuffer cb;
   VkCommandBufferAllocateInfo cai = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
                                       .commandPool = pool, .commandBufferCount = 1 };
   CK(vkAllocateCommandBuffers(dev, &cai, &cb));

   struct img frame[2], out;
   struct img pyr[2][LEVELS], flow[LEVELS], flow2[LEVELS];
   for (int h = 0; h < 2; h++) {
      frame[h] = mkimg(VK_FORMAT_R8G8B8A8_UNORM, W, H);
      for (int l = 1; l < LEVELS; l++)
         pyr[h][l] = mkimg(VK_FORMAT_R16_SFLOAT, (W + (1 << l) - 1) >> l, (H + (1 << l) - 1) >> l);
   }
   for (int l = 1; l < LEVELS; l++) {
      flow[l] = mkimg(VK_FORMAT_R16G16B16A16_SFLOAT, (W + (1 << l) - 1) >> l, (H + (1 << l) - 1) >> l);
      flow2[l] = mkimg(VK_FORMAT_R16G16B16A16_SFLOAT, (W + (1 << l) - 1) >> l, (H + (1 << l) - 1) >> l);
   }
   /* FG_PROPAGATE: propagation passes per level (0 = none) */
   const int npropagate = getenv("FG_PROPAGATE") ? atoi(getenv("FG_PROPAGATE")) : 1;
   out = mkimg(VK_FORMAT_R8G8B8A8_UNORM, W, H);
   struct buf up = mkbuf((VkDeviceSize)W * H * 4 * 3), down = mkbuf((VkDeviceSize)W * H * 4);
   struct buf fdown = mkbuf((VkDeviceSize)W * H * 8);
   VkQueryPoolCreateInfo qpi = { VK_STRUCTURE_TYPE_QUERY_POOL_CREATE_INFO,
                                 .queryType = VK_QUERY_TYPE_TIMESTAMP, .queryCount = 4 };
   VkQueryPool qp;
   CK(vkCreateQueryPool(dev, &qpi, NULL, &qp));

   uint8_t *truth = malloc(W * H * 4), *hud = malloc(W * H), *gen = down.p;
   uint8_t *moving = malloc(W * H);
   double sum_psnr = 0, sum_edge = 0, sum_hud = 0;
   double sum_epe = 0;
   printf("%-12s %8s %8s %8s %8s %8s %8s\n", "scene", "all dB", "edge", "hud", "us", "EPE px", "bad %");
   for (int s = 0; s < SC_COUNT; s++) {
      render(s, 0.0f, (uint8_t *)up.p, NULL);
      render(s, 1.0f, (uint8_t *)up.p + W * H * 4, NULL);
      render(s, 0.5f, truth, hud);
      /* the edge band: pixels within 12 px of where the frames differ at
       * t=0, 0.5 or 1 (a moving edge) */
      {
         uint8_t *t0 = up.p, *t1 = (uint8_t *)up.p + W * H * 4;
         uint8_t *d = calloc(W * H, 1);
         for (int i = 0; i < W * H; i++)
            d[i] = !hud[i] && (abs(t0[4 * i] - truth[4 * i]) + abs(t1[4 * i] - truth[4 * i]) +
                               abs(t0[4 * i + 1] - t1[4 * i + 1])) > 30;
         memset(moving, 0, W * H);
         for (int y = 0; y < H; y++)
            for (int x = 0; x < W; x++) {
               if (!d[y * W + x]) continue;
               for (int dy = -12; dy <= 12; dy += 3)
                  for (int dx = -12; dx <= 12; dx += 3) {
                     int xx = x + dx, yy = y + dy;
                     if (xx >= 0 && yy >= 0 && xx < W && yy < H && !hud[yy * W + xx])
                        moving[yy * W + xx] = 1;
                  }
            }
         free(d);
      }

      VkCommandBufferBeginInfo bi = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO };
      CK(vkBeginCommandBuffer(cb, &bi));
      vkCmdResetQueryPool(cb, qp, 0, 4);
      VkImageMemoryBarrier ib[64];
      int nb = 0;
      struct img *all[] = { &frame[0], &frame[1], &out };
      for (int i = 0; i < 3; i++)
         ib[nb++] = (VkImageMemoryBarrier){ VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
            .dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT | VK_ACCESS_SHADER_WRITE_BIT,
            .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED, .newLayout = VK_IMAGE_LAYOUT_GENERAL,
            .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
            .image = all[i]->i, .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
      for (int h = 0; h < 2; h++)
         for (int l = 1; l < LEVELS; l++)
            ib[nb++] = (VkImageMemoryBarrier){ VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
               .dstAccessMask = VK_ACCESS_SHADER_WRITE_BIT, .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
               .newLayout = VK_IMAGE_LAYOUT_GENERAL, .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
               .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .image = (h ? pyr[1] : pyr[0])[l].i,
               .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
      for (int l = 1; l < LEVELS; l++)
         ib[nb++] = (VkImageMemoryBarrier){ VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
            .dstAccessMask = VK_ACCESS_SHADER_WRITE_BIT, .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
            .newLayout = VK_IMAGE_LAYOUT_GENERAL, .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
            .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .image = flow[l].i,
            .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
      for (int l = 1; l < LEVELS; l++)
         ib[nb++] = (VkImageMemoryBarrier){ VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
            .dstAccessMask = VK_ACCESS_SHADER_WRITE_BIT, .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
            .newLayout = VK_IMAGE_LAYOUT_GENERAL, .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
            .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .image = flow2[l].i,
            .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
      vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT,
                           0, 0, NULL, 0, NULL, nb, ib);
      for (int h = 0; h < 2; h++) {
         VkBufferImageCopy c = { .bufferOffset = (VkDeviceSize)h * W * H * 4,
            .imageSubresource = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1 }, .imageExtent = { W, H, 1 } };
         vkCmdCopyBufferToImage(cb, up.b, frame[h].i, VK_IMAGE_LAYOUT_GENERAL, 1, &c);
      }
      barrier(cb);
      vkCmdWriteTimestamp(cb, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, qp, 0);
      struct push pc;
      /* the pyramids (in the driver, the previous frame's comes from its own present) */
      for (int h = 0; h < 2; h++) {
         for (int l = 1; l < LEVELS; l++) {
            memset(&pc, 0, sizeof(pc));
            struct img *from = l == 1 ? &frame[h] : &pyr[h][l - 1];
            pc.src[0] = from->w; pc.src[1] = from->h;
            pc.dst[0] = pyr[h][l].w; pc.dst[1] = pyr[h][l].h;
            pc.mode = l == 1;
            dispatch(cb, p_down, from->v, NULL, NULL, pyr[h][l].v, &pc);
            barrier(cb);
         }
      }
      vkCmdWriteTimestamp(cb, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, qp, 1);
      const int finest = getenv("FG_FINEST") ? atoi(getenv("FG_FINEST")) : hq ? 1 : 2;
      const int coarsest = getenv("FG_COARSEST") ? atoi(getenv("FG_COARSEST")) : 5;
      for (int l = coarsest; l >= finest; l--) {
         memset(&pc, 0, sizeof(pc));
         pc.src[0] = pyr[1][l].w; pc.src[1] = pyr[1][l].h;
         pc.dst[0] = flow[l].w; pc.dst[1] = flow[l].h;
         pc.p0[0] = (float)(1 << l);
         pc.p0[1] = 4.0f;
         pc.p0[2] = getenv("FG_LAMBDA") ? (float)atof(getenv("FG_LAMBDA")) : 0.0003f;
         pc.mode = l != coarsest;
         if (pc.mode)
            pc.p0[3] = getenv("FG_REFINE") ? (float)atof(getenv("FG_REFINE")) : 0.0f;
         if (l < coarsest) { pc.aux[0] = flow[l + 1].w; pc.aux[1] = flow[l + 1].h; }
         dispatch(cb, p_flow, pyr[0][l].v, pyr[1][l].v, l < coarsest ? flow[l + 1].v : pyr[1][l].v,
                  flow[l].v, &pc);
         barrier(cb);
         /* the driver: from 1/8 down at fast, everywhere at quality */
         const int prop_from = getenv("FG_PROPAGATE_FROM") ? atoi(getenv("FG_PROPAGATE_FROM")) : hq ? 5 : 3;
         const bool prop_here = l <= prop_from;
         for (int it = 0; prop_here && it < npropagate; it++) {
            /* flow -> flow2 -> flow */
            pc.mode = 2;
            pc.aux[0] = flow[l].w; pc.aux[1] = flow[l].h;
            dispatch(cb, p_flow, pyr[0][l].v, pyr[1][l].v, flow[l].v, flow2[l].v, &pc);
            barrier(cb);
            dispatch(cb, p_flow, pyr[0][l].v, pyr[1][l].v, flow2[l].v, flow[l].v, &pc);
            barrier(cb);
         }
      }
      vkCmdWriteTimestamp(cb, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, qp, 2);
      if (getenv("FG_TRUE_FLOW")) {
         /* the finest level from the scene itself: what interpolation
          * makes of a perfect flow */
         uint16_t *fl = (uint16_t *)((uint8_t *)up.p + (VkDeviceSize)W * H * 4 * 2);
         const float sc = (float)(1 << finest);
         for (uint32_t y = 0; y < flow[finest].h; y++)
            for (uint32_t x = 0; x < flow[finest].w; x++) {
               float mx, my;
               true_motion(s, (x + 0.5f) * sc, (y + 0.5f) * sc, &mx, &my);
               uint16_t *o = fl + 4 * (y * flow[finest].w + x);
               o[0] = f2h(mx); o[1] = f2h(my); o[2] = 0; o[3] = f2h(1.0f);
            }
         VkBufferImageCopy c2 = { .bufferOffset = (VkDeviceSize)W * H * 4 * 2,
            .imageSubresource = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1 },
            .imageExtent = { flow[finest].w, flow[finest].h, 1 } };
         vkCmdCopyBufferToImage(cb, up.b, flow[finest].i, VK_IMAGE_LAYOUT_GENERAL, 1, &c2);
         barrier(cb);
      }
      memset(&pc, 0, sizeof(pc));
      pc.src[0] = W; pc.src[1] = H; pc.dst[0] = W; pc.dst[1] = H;
      pc.aux[0] = flow[finest].w; pc.aux[1] = flow[finest].h;
      pc.p0[0] = 0.5f;
      pc.p0[1] = (float)(1 << finest);
      pc.p1[0] = pc.p1[1] = 1.0f;
      pc.mode = interp_mode;
      dispatch(cb, p_interp, frame[0].v, frame[1].v, flow[finest].v, out.v, &pc);
      vkCmdWriteTimestamp(cb, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, qp, 3);
      barrier(cb);
      VkBufferImageCopy c = { .imageSubresource = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1 },
                              .imageExtent = { W, H, 1 } };
      vkCmdCopyImageToBuffer(cb, out.i, VK_IMAGE_LAYOUT_GENERAL, down.b, 1, &c);
      VkBufferImageCopy cf = { .imageSubresource = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1 },
                               .imageExtent = { flow[finest].w, flow[finest].h, 1 } };
      vkCmdCopyImageToBuffer(cb, flow[finest].i, VK_IMAGE_LAYOUT_GENERAL, fdown.b, 1, &cf);
      CK(vkEndCommandBuffer(cb));
      VkSubmitInfo si = { VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cb };
      CK(vkQueueSubmit(queue, 1, &si, VK_NULL_HANDLE));
      CK(vkQueueWaitIdle(queue));
      uint64_t ts[4];
      vkGetQueryPoolResults(dev, qp, 0, 4, sizeof(ts), ts, 8, VK_QUERY_RESULT_64_BIT);
      /* one frame's pyramid is the driver's cost: half of what ran here */
      const double us = ((ts[3] - ts[2]) + (ts[2] - ts[1]) + (ts[1] - ts[0]) / 2.0) *
                        props.limits.timestampPeriod / 1000.0;

      double se = 0, edge = 0, hudv = 0;
      long ne = 0, nh = 0;
      uint8_t *diff = malloc(W * H * 4);
      for (int i = 0; i < W * H; i++) {
         double e = 0;
         for (int k = 0; k < 3; k++) {
            const double d = (double)gen[4 * i + k] - truth[4 * i + k];
            se += d * d;
            e += fabs(d);
         }
         e /= 3.0;
         diff[4 * i] = diff[4 * i + 1] = diff[4 * i + 2] = (uint8_t)fmin(255.0, e * 4);
         if (moving[i]) { edge += e; ne++; }
         if (hud[i]) { hudv += e; nh++; }
      }
      /* flow endpoint error at the finest level, outside the HUD */
      double epe = 0, bad = 0;
      long nf = 0;
      {
         const uint16_t *fl = fdown.p;
         const float sc = (float)(1 << finest);
         for (uint32_t y = 0; y < flow[finest].h; y++)
            for (uint32_t x = 0; x < flow[finest].w; x++) {
               float mx, my;
               const float px = (x + 0.5f) * sc, py = (y + 0.5f) * sc;
               if (px >= W || py >= H || hud[(int)py * W + (int)px])
                  continue;
               true_motion(s, px, py, &mx, &my);
               const uint16_t *o = fl + 4 * (y * flow[finest].w + x);
               const float e = hypotf(h2f(o[0]) - mx, h2f(o[1]) - my);
               epe += e;
               bad += e > 3.0f;
               nf++;
            }
      }
      const double mse = se / (W * H * 3.0);
      const double psnr = 10.0 * log10(255.0 * 255.0 / (mse > 1e-9 ? mse : 1e-9));
      printf("%-12s %8.2f %8.2f %8.2f %8.0f %8.2f %8.1f\n", scene_name[s], psnr, ne ? edge / ne : 0,
             nh ? hudv / nh : 0, us, nf ? epe / nf : 0, nf ? 100.0 * bad / nf : 0);
      sum_epe += nf ? epe / nf : 0;
      sum_psnr += psnr;
      sum_edge += ne ? edge / ne : 0;
      sum_hud += nh ? hudv / nh : 0;
      if (out_dir) {
         char path[512];
         snprintf(path, sizeof(path), "%s/%s-gen.ppm", out_dir, scene_name[s]);
         write_ppm(path, gen);
         snprintf(path, sizeof(path), "%s/%s-truth.ppm", out_dir, scene_name[s]);
         write_ppm(path, truth);
         snprintf(path, sizeof(path), "%s/%s-diff.ppm", out_dir, scene_name[s]);
         write_ppm(path, diff);
      }
      free(diff);
   }
   printf("%-12s %8.2f %8.2f %8.2f %8s %8.2f\n", "mean", sum_psnr / SC_COUNT, sum_edge / SC_COUNT,
          sum_hud / SC_COUNT, "", sum_epe / SC_COUNT);
   vkDeviceWaitIdle(dev);
   return 0;
}
