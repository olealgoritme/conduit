/* vk_vb_rebind_test: a vertex binding that is null in one draw and a real
 * buffer in another, the way DXVK binds CS2's D3D11 input slot 2.
 *
 * CS2 draws with b0 per-vertex positions (stride 12), b1 a per-instance
 * R32_UINT (stride 4, divisor 1) and b2 an RGBA32UI attribute that is
 * unbound in some draws (robustness2 nullDescriptor: VK_NULL_HANDLE, size 0,
 * stride 0) and a real buffer in others.  Every step binds b2 as null or as
 * a real buffer (device-local or host-visible, at offsets, with explicit or
 * whole sizes, through all three bindings or firstBinding 2 alone, through
 * vkCmdBindVertexBuffers2 with dynamic strides or vkCmdBindVertexBuffers
 * with static ones), draws points with rasterizer discard, and the vertex
 * shader appends what it fetched.  The CPU checks every record against the
 * binding that was current for its draw: a null b2 must read zeros, a real
 * b2 the buffer's words.  Direct, indexed and indirect draws, per-vertex and
 * per-instance b2 (divisor 0 and 1), pipeline switches, a meta blit and a
 * second command buffer.  A few hundred vertices: microseconds of GPU time.
 */
#ifdef _WIN32
#include <windows.h>
#endif
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>

#include "vb_rebind_vert.h"

#define CHECK(x) do { VkResult r_ = (x); if (r_ != VK_SUCCESS) { \
   fprintf(stderr, "FAIL %s:%d: %s -> %d\n", __FILE__, __LINE__, #x, r_); exit(1); } } while (0)

#define MAXREC 16384
#define NTAG 64
#define RBYTES 65536u

static VkPhysicalDeviceMemoryProperties memprops;
static VkDevice dev;

static uint32_t find_mem(uint32_t bits, VkMemoryPropertyFlags want)
{
   for (uint32_t i = 0; i < memprops.memoryTypeCount; i++)
      if ((bits & (1u << i)) && (memprops.memoryTypes[i].propertyFlags & want) == want)
         return i;
   fprintf(stderr, "FAIL no memory type\n");
   exit(1);
}

static VkBuffer make_buf(VkDeviceSize size, VkBufferUsageFlags usage, VkMemoryPropertyFlags want,
                         void **map)
{
   VkBufferCreateInfo bci = { VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size = size, .usage = usage };
   VkBuffer b;
   CHECK(vkCreateBuffer(dev, &bci, NULL, &b));
   VkMemoryRequirements mr;
   vkGetBufferMemoryRequirements(dev, b, &mr);
   VkMemoryAllocateInfo mai = { VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = mr.size,
      .memoryTypeIndex = find_mem(mr.memoryTypeBits, want) };
   VkDeviceMemory m;
   CHECK(vkAllocateMemory(dev, &mai, NULL, &m));
   CHECK(vkBindBufferMemory(dev, b, m, 0));
   if (map)
      CHECK(vkMapMemory(dev, m, 0, size, 0, map));
   return b;
}

static VkImage make_img(void)
{
   VkImageCreateInfo ii = { VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO, .imageType = VK_IMAGE_TYPE_2D,
      .format = VK_FORMAT_R8G8B8A8_UNORM, .extent = { 16, 16, 1 }, .mipLevels = 1, .arrayLayers = 1,
      .samples = VK_SAMPLE_COUNT_1_BIT, .tiling = VK_IMAGE_TILING_OPTIMAL,
      .usage = VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT };
   VkImage img;
   CHECK(vkCreateImage(dev, &ii, NULL, &img));
   VkMemoryRequirements mr;
   vkGetImageMemoryRequirements(dev, img, &mr);
   VkMemoryAllocateInfo mai = { VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = mr.size,
      .memoryTypeIndex = find_mem(mr.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT) };
   VkDeviceMemory m;
   CHECK(vkAllocateMemory(dev, &mai, NULL, &m));
   CHECK(vkBindImageMemory(dev, img, m, 0));
   return img;
}

/* What b2 is for a tag's draw */
struct b2_state {
   const uint32_t *words;   /* NULL: null binding */
   uint32_t off, range;     /* bytes: bound offset, bound size */
   uint32_t stride;
   int instance;
   uint32_t divisor;
};

struct tag_info {
   char what[96];
   struct b2_state b2;
   uint32_t expected, seen, bad;
};

static struct tag_info tags[NTAG];
static uint32_t ntags;

/* Pipelines: b2 rate/divisor and whether strides are dynamic */
enum { P_INST0_DYN, P_VERT_DYN, P_INST1_DYN, P_INST0_STATIC, P_VERT16_STATIC, NPIPE };
static const struct { int instance; uint32_t divisor; int dyn; uint32_t b2_stride; } pdesc[NPIPE] = {
   [P_INST0_DYN]     = { 1, 0, 1, 0 },
   [P_VERT_DYN]      = { 0, 1, 1, 0 },
   [P_INST1_DYN]     = { 1, 1, 1, 0 },
   [P_INST0_STATIC]  = { 1, 0, 0, 0 },
   [P_VERT16_STATIC] = { 0, 1, 0, 16 },
};

static VkCommandBuffer cb;
static VkPipeline pipe[NPIPE];
static VkPipelineLayout pl;
static VkBuffer pos_buf, inst_buf;
static int cur_pipe = -1;
static struct b2_state cur_b2;

/* DXVK's vkCmdBindVertexBuffers2: all bindings, sizes given, strides when
 * the pipeline's are dynamic (NULL otherwise) */
static void bind_all(VkBuffer b2, const uint32_t *words, uint32_t off, uint32_t size,
                     uint32_t stride, int dyn)
{
   VkBuffer bufs[3] = { pos_buf, inst_buf, b2 };
   VkDeviceSize offs[3] = { 0, 0, b2 ? off : 0 };
   VkDeviceSize sizes[3] = { 4096 * 12, 4096 * 4, b2 ? size : 0 };
   VkDeviceSize strides[3] = { 12, 4, b2 ? stride : 0 };
   vkCmdBindVertexBuffers2(cb, 0, 3, bufs, offs, sizes, dyn ? strides : NULL);
   cur_b2 = (struct b2_state){ b2 ? words : NULL, off, size, stride, 0, 0 };
}

/* firstBinding 2 alone; size NULL means VK_WHOLE_SIZE */
static void bind_b2(VkBuffer b2, const uint32_t *words, uint32_t off, int with_size, uint32_t size,
                    uint32_t stride, int dyn)
{
   VkDeviceSize o = b2 ? off : 0, s = b2 ? size : 0, st = b2 ? stride : 0;
   vkCmdBindVertexBuffers2(cb, 2, 1, &b2, &o, with_size ? &s : NULL, dyn ? &st : NULL);
   cur_b2 = (struct b2_state){ b2 ? words : NULL, off, with_size ? size : RBYTES - off, stride, 0, 0 };
}

/* vkCmdBindVertexBuffers (static strides) */
static void bind_v1(VkBuffer b2, const uint32_t *words, uint32_t off)
{
   VkBuffer bufs[3] = { pos_buf, inst_buf, b2 };
   VkDeviceSize offs[3] = { 0, 0, b2 ? off : 0 };
   vkCmdBindVertexBuffers(cb, 0, 3, bufs, offs);
   cur_b2 = (struct b2_state){ b2 ? words : NULL, off, RBYTES - off, 0, 0, 0 };
}

static void tag(const char *what, int p, uint32_t invocations)
{
   struct tag_info *t = &tags[ntags];
   snprintf(t->what, sizeof(t->what), "%s", what);
   t->b2 = cur_b2;
   t->b2.instance = pdesc[p].instance;
   t->b2.divisor = pdesc[p].divisor;
   if (!pdesc[p].dyn)
      t->b2.stride = pdesc[p].b2_stride;
   t->expected = invocations;
   if (cur_pipe != p) {
      vkCmdBindPipeline(cb, VK_PIPELINE_BIND_POINT_GRAPHICS, pipe[p]);
      cur_pipe = p;
   }
   uint32_t pc[4] = { ntags, 0, 0, 0 };
   vkCmdPushConstants(cb, pl, VK_SHADER_STAGE_VERTEX_BIT, 0, 16, pc);
   ntags++;
}

static void expect_b2(const struct b2_state *b, uint32_t vi, uint32_t ii, uint32_t bi, uint32_t e[4],
                      int *oob)
{
   *oob = 0;
   memset(e, 0, 16);
   if (!b->words)
      return;
   uint64_t elem;
   if (!b->instance)
      elem = vi;
   else if (b->divisor == 0)
      elem = bi;
   else
      elem = bi + (ii - bi) / b->divisor;
   uint64_t byte = elem * b->stride;
   if (byte + 16 > b->range) {
      *oob = 1;
      return;
   }
   for (int c = 0; c < 4; c++)
      e[c] = b->words[(b->off + byte) / 4 + c];
}

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

   uint32_t next = 0;
   vkEnumerateDeviceExtensionProperties(pd, NULL, &next, NULL);
   VkExtensionProperties *exts = calloc(next, sizeof(*exts));
   vkEnumerateDeviceExtensionProperties(pd, NULL, &next, exts);
   int khr_r2 = 0, khr_div = 0;
   for (uint32_t i = 0; i < next; i++) {
      khr_r2 |= !strcmp(exts[i].extensionName, "VK_KHR_robustness2");
      khr_div |= !strcmp(exts[i].extensionName, "VK_KHR_vertex_attribute_divisor");
   }
   const char *dev_exts[] = {
      khr_r2 ? "VK_KHR_robustness2" : "VK_EXT_robustness2",
      khr_div ? "VK_KHR_vertex_attribute_divisor" : "VK_EXT_vertex_attribute_divisor",
      "VK_EXT_extended_dynamic_state",
   };

   uint32_t qf = 0, nqf = 16;
   VkQueueFamilyProperties qfp[16];
   vkGetPhysicalDeviceQueueFamilyProperties(pd, &nqf, qfp);
   for (qf = 0; qf < nqf; qf++)
      if (qfp[qf].queueFlags & VK_QUEUE_GRAPHICS_BIT)
         break;

   /* The features DXVK enables that matter here */
   VkPhysicalDeviceVertexAttributeDivisorFeaturesKHR divf = {
      VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VERTEX_ATTRIBUTE_DIVISOR_FEATURES_KHR,
      .vertexAttributeInstanceRateDivisor = VK_TRUE, .vertexAttributeInstanceRateZeroDivisor = VK_TRUE };
   VkPhysicalDeviceRobustness2FeaturesEXT r2 = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ROBUSTNESS_2_FEATURES_EXT,
      &divf, .robustBufferAccess2 = VK_TRUE, .robustImageAccess2 = VK_TRUE, .nullDescriptor = VK_TRUE };
   VkPhysicalDeviceExtendedDynamicStateFeaturesEXT eds = {
      VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTENDED_DYNAMIC_STATE_FEATURES_EXT, &r2, .extendedDynamicState = VK_TRUE };
   VkPhysicalDeviceVulkan13Features f13 = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES, &eds,
      .dynamicRendering = VK_TRUE, .robustImageAccess = VK_TRUE };
   VkPhysicalDeviceVulkan11Features f11 = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_1_FEATURES, &f13,
      .shaderDrawParameters = VK_TRUE };
   VkPhysicalDeviceFeatures2 f2 = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2, &f11 };
   f2.features.robustBufferAccess = VK_TRUE;
   f2.features.vertexPipelineStoresAndAtomics = VK_TRUE;
   f2.features.multiDrawIndirect = VK_TRUE;
   f2.features.drawIndirectFirstInstance = VK_TRUE;

   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = { VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = qf, .queueCount = 1, .pQueuePriorities = &prio };
   VkDeviceCreateInfo dci = { VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, &f2,
      .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci,
      .enabledExtensionCount = 3, .ppEnabledExtensionNames = dev_exts };
   CHECK(vkCreateDevice(pd, &dci, NULL, &dev));
   VkQueue q;
   vkGetDeviceQueue(dev, qf, 0, &q);

   const VkMemoryPropertyFlags hv = VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                                    VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;
   void *m_out, *m_pos, *m_inst, *m_idx, *m_ind, *m_r2, *m_stage;
   VkBuffer out_buf = make_buf(16 + (VkDeviceSize)MAXREC * 64, VK_BUFFER_USAGE_STORAGE_BUFFER_BIT |
                               VK_BUFFER_USAGE_TRANSFER_DST_BIT, hv, &m_out);
   pos_buf = make_buf(4096 * 12, VK_BUFFER_USAGE_VERTEX_BUFFER_BIT, hv, &m_pos);
   inst_buf = make_buf(4096 * 4, VK_BUFFER_USAGE_VERTEX_BUFFER_BIT, hv, &m_inst);
   VkBuffer idx_buf = make_buf(256 * 4, VK_BUFFER_USAGE_INDEX_BUFFER_BIT, hv, &m_idx);
   VkBuffer ind_buf = make_buf(4096, VK_BUFFER_USAGE_INDIRECT_BUFFER_BIT, hv, &m_ind);
   /* b2's real buffers: R1 device-local (filled by a copy, as CS2's are), R2
    * host-visible */
   VkBuffer r1 = make_buf(RBYTES, VK_BUFFER_USAGE_VERTEX_BUFFER_BIT | VK_BUFFER_USAGE_TRANSFER_DST_BIT,
                          VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT, NULL);
   VkBuffer r2b = make_buf(RBYTES, VK_BUFFER_USAGE_VERTEX_BUFFER_BIT, hv, &m_r2);
   VkBuffer stage = make_buf(RBYTES, VK_BUFFER_USAGE_TRANSFER_SRC_BIT, hv, &m_stage);
   static uint32_t w1[RBYTES / 4], w2[RBYTES / 4];
   for (uint32_t i = 0; i < RBYTES / 4; i++) {
      w1[i] = 0xB1000000u | i;
      w2[i] = 0xB2000000u | i;
   }
   memcpy(m_stage, w1, RBYTES);
   memcpy(m_r2, w2, RBYTES);
   memset(m_out, 0, 16 + (size_t)MAXREC * 64);
   uint32_t *pos = m_pos, *ib = m_inst, *idx = m_idx;
   for (uint32_t i = 0; i < 4096; i++) {
      pos[i * 3 + 0] = i;
      pos[i * 3 + 1] = 0;
      pos[i * 3 + 2] = 7;
      ib[i] = i + 0x10000u;
   }
   for (uint32_t i = 0; i < 256; i++)
      idx[i] = (i * 7) % 64;

   VkDescriptorSetLayoutBinding b0 = { 0, VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 1, VK_SHADER_STAGE_VERTEX_BIT };
   VkDescriptorSetLayoutCreateInfo dslci = { VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
      .bindingCount = 1, .pBindings = &b0 };
   VkDescriptorSetLayout dsl;
   CHECK(vkCreateDescriptorSetLayout(dev, &dslci, NULL, &dsl));
   VkDescriptorPoolSize ps = { VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 1 };
   VkDescriptorPoolCreateInfo dpci = { VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
      .maxSets = 1, .poolSizeCount = 1, .pPoolSizes = &ps };
   VkDescriptorPool dp;
   CHECK(vkCreateDescriptorPool(dev, &dpci, NULL, &dp));
   VkDescriptorSetAllocateInfo dsai = { VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
      .descriptorPool = dp, .descriptorSetCount = 1, .pSetLayouts = &dsl };
   VkDescriptorSet ds;
   CHECK(vkAllocateDescriptorSets(dev, &dsai, &ds));
   VkDescriptorBufferInfo obi = { out_buf, 0, VK_WHOLE_SIZE };
   VkWriteDescriptorSet w = { VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = ds, .dstBinding = 0,
      .descriptorCount = 1, .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, .pBufferInfo = &obi };
   vkUpdateDescriptorSets(dev, 1, &w, 0, NULL);

   VkPushConstantRange pcr = { VK_SHADER_STAGE_VERTEX_BIT, 0, 16 };
   VkPipelineLayoutCreateInfo plci = { VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
      .setLayoutCount = 1, .pSetLayouts = &dsl, .pushConstantRangeCount = 1, .pPushConstantRanges = &pcr };
   CHECK(vkCreatePipelineLayout(dev, &plci, NULL, &pl));
   VkShaderModuleCreateInfo smci = { VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
      .codeSize = sizeof(vb_rebind_vert), .pCode = vb_rebind_vert };
   VkShaderModule vs;
   CHECK(vkCreateShaderModule(dev, &smci, NULL, &vs));

   VkVertexInputAttributeDescription va[4] = {
      { 0, 0, VK_FORMAT_R32G32B32_UINT, 0 },
      { 1, 1, VK_FORMAT_R32_UINT, 0 },
      { 2, 2, VK_FORMAT_R32G32B32A32_UINT, 0 },
      { 3, 2, VK_FORMAT_R32G32B32A32_SFLOAT, 0 },
   };
   VkPipelineInputAssemblyStateCreateInfo ias = { VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
      .topology = VK_PRIMITIVE_TOPOLOGY_POINT_LIST };
   VkViewport vp = { 0, 0, 16, 16, 0, 1 };
   VkRect2D sc = { { 0, 0 }, { 16, 16 } };
   VkPipelineViewportStateCreateInfo vps = { VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO,
      .viewportCount = 1, .pViewports = &vp, .scissorCount = 1, .pScissors = &sc };
   VkPipelineRasterizationStateCreateInfo rs = { VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO,
      .rasterizerDiscardEnable = VK_TRUE, .lineWidth = 1.0f };
   VkDynamicState dyn[] = { VK_DYNAMIC_STATE_VERTEX_INPUT_BINDING_STRIDE };
   VkPipelineRenderingCreateInfo prci = { VK_STRUCTURE_TYPE_PIPELINE_RENDERING_CREATE_INFO };
   VkPipelineShaderStageCreateInfo st = { VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
      .stage = VK_SHADER_STAGE_VERTEX_BIT, .module = vs, .pName = "main" };
   for (uint32_t i = 0; i < NPIPE; i++) {
      VkVertexInputBindingDescription vb[3] = {
         { 0, 12, VK_VERTEX_INPUT_RATE_VERTEX },
         { 1, 4, VK_VERTEX_INPUT_RATE_INSTANCE },
         { 2, pdesc[i].b2_stride,
           pdesc[i].instance ? VK_VERTEX_INPUT_RATE_INSTANCE : VK_VERTEX_INPUT_RATE_VERTEX },
      };
      VkVertexInputBindingDivisorDescriptionKHR div[2] = { { 1, 1 }, { 2, pdesc[i].divisor } };
      VkPipelineVertexInputDivisorStateCreateInfoKHR divs = {
         VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_DIVISOR_STATE_CREATE_INFO_KHR,
         .vertexBindingDivisorCount = pdesc[i].instance ? 2 : 1, .pVertexBindingDivisors = div };
      VkPipelineVertexInputStateCreateInfo vis = {
         VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO, &divs,
         .vertexBindingDescriptionCount = 3, .pVertexBindingDescriptions = vb,
         .vertexAttributeDescriptionCount = 4, .pVertexAttributeDescriptions = va };
      VkPipelineDynamicStateCreateInfo dys = { VK_STRUCTURE_TYPE_PIPELINE_DYNAMIC_STATE_CREATE_INFO,
         .dynamicStateCount = pdesc[i].dyn ? 1 : 0, .pDynamicStates = dyn };
      VkGraphicsPipelineCreateInfo gpci = { VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO, &prci,
         .stageCount = 1, .pStages = &st, .pVertexInputState = &vis, .pInputAssemblyState = &ias,
         .pViewportState = &vps, .pRasterizationState = &rs, .pDynamicState = &dys, .layout = pl };
      CHECK(vkCreateGraphicsPipelines(dev, VK_NULL_HANDLE, 1, &gpci, NULL, &pipe[i]));
   }

   VkImage img_a = make_img(), img_b = make_img();

   VkCommandPoolCreateInfo cpi = { VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .queueFamilyIndex = qf };
   VkCommandPool pool;
   CHECK(vkCreateCommandPool(dev, &cpi, NULL, &pool));
   VkCommandBufferAllocateInfo cbai = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
      .commandPool = pool, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 2 };
   VkCommandBuffer cbs[2];
   CHECK(vkAllocateCommandBuffers(dev, &cbai, cbs));
   VkCommandBufferBeginInfo cbbi = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
      .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT };
   VkRenderingInfo ri = { VK_STRUCTURE_TYPE_RENDERING_INFO, .renderArea = { { 0, 0 }, { 16, 16 } },
      .layerCount = 1 };
   VkDrawIndexedIndirectCommand *ind = m_ind;

   /* ---- command buffer 1 ---- */
   cb = cbs[0];
   CHECK(vkBeginCommandBuffer(cb, &cbbi));
   VkBufferCopy cp = { 0, 0, RBYTES };
   vkCmdCopyBuffer(cb, stage, r1, 1, &cp);
   VkImageMemoryBarrier imb[2];
   for (int i = 0; i < 2; i++) {
      imb[i] = (VkImageMemoryBarrier){ VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
         .dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT | VK_ACCESS_TRANSFER_WRITE_BIT,
         .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED, .newLayout = VK_IMAGE_LAYOUT_GENERAL,
         .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
         .image = i ? img_b : img_a, .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
   }
   VkMemoryBarrier upl = { VK_STRUCTURE_TYPE_MEMORY_BARRIER, .srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
      .dstAccessMask = VK_ACCESS_VERTEX_ATTRIBUTE_READ_BIT };
   vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TRANSFER_BIT,
                        VK_PIPELINE_STAGE_VERTEX_INPUT_BIT | VK_PIPELINE_STAGE_TRANSFER_BIT,
                        0, 1, &upl, 0, NULL, 2, imb);

   vkCmdBeginRendering(cb, &ri);
   vkCmdBindDescriptorSets(cb, VK_PIPELINE_BIND_POINT_GRAPHICS, pl, 0, 1, &ds, 0, NULL);
   vkCmdBindIndexBuffer(cb, idx_buf, 0, VK_INDEX_TYPE_UINT32);

   /* CS2's prepass binding: b2 null, then real, then null again */
   bind_all(VK_NULL_HANDLE, NULL, 0, 0, 0, 1);
   tag("null b2, draw 4 x2 fi 5", P_INST0_DYN, 8);
   vkCmdDraw(cb, 4, 2, 0, 5);
   bind_all(r1, w1, 0, 16, 0, 1);
   tag("R1 b2 size 16 stride 0, draw 4 x2 fi 5", P_INST0_DYN, 8);
   vkCmdDraw(cb, 4, 2, 0, 5);
   bind_all(VK_NULL_HANDLE, NULL, 0, 0, 0, 1);
   tag("null b2 again, indexed 8 x1", P_INST0_DYN, 8);
   vkCmdDrawIndexed(cb, 8, 1, 0, 0, 0);
   bind_all(r1, w1, 256, 16, 0, 1);
   ind[0] = (VkDrawIndexedIndirectCommand){ 8, 1, 0, 0, 1236 };
   tag("R1+256 b2, indexed indirect fi 1236", P_INST0_DYN, 8);
   vkCmdDrawIndexedIndirect(cb, ind_buf, 0, 1, sizeof(*ind));

   /* firstBinding 2 alone */
   bind_b2(VK_NULL_HANDLE, NULL, 0, 1, 0, 0, 1);
   tag("firstBinding 2: null b2", P_INST0_DYN, 4);
   vkCmdDraw(cb, 4, 1, 0, 0);
   bind_b2(r1, w1, 32, 0, 0, 0, 1);
   tag("firstBinding 2: R1+32 whole size", P_INST0_DYN, 4);
   vkCmdDraw(cb, 4, 1, 0, 0);
   bind_b2(r2b, w2, 48, 1, 16, 0, 1);
   tag("firstBinding 2: R2+48 size 16 (host-visible)", P_INST0_DYN, 6);
   vkCmdDrawIndexed(cb, 3, 2, 0, 0, 300);
   bind_b2(VK_NULL_HANDLE, NULL, 0, 1, 0, 0, 1);
   tag("firstBinding 2: null b2 after R2", P_INST0_DYN, 4);
   vkCmdDraw(cb, 4, 1, 0, 0);

   /* the same real range twice in a row, and null twice in a row */
   bind_all(r1, w1, 512, 4096, 0, 1);
   tag("R1+512 size 4096", P_INST0_DYN, 4);
   vkCmdDraw(cb, 4, 1, 0, 0);
   bind_all(r1, w1, 512, 4096, 0, 1);
   tag("R1+512 size 4096 again", P_INST0_DYN, 4);
   vkCmdDraw(cb, 4, 1, 0, 0);
   bind_all(VK_NULL_HANDLE, NULL, 0, 0, 0, 1);
   bind_all(VK_NULL_HANDLE, NULL, 0, 0, 0, 1);
   tag("null twice", P_INST0_DYN, 4);
   vkCmdDraw(cb, 4, 1, 0, 0);

   /* b2 per vertex, stride 16, after null */
   bind_all(r1, w1, 0, RBYTES, 16, 1);
   tag("vertex-rate R1 stride 16, draw 8 first_vertex 3", P_VERT_DYN, 8);
   vkCmdDraw(cb, 8, 1, 3, 0);
   bind_all(VK_NULL_HANDLE, NULL, 0, 0, 0, 1);
   tag("vertex-rate null", P_VERT_DYN, 8);
   vkCmdDraw(cb, 8, 1, 3, 0);
   bind_all(r1, w1, 64, RBYTES - 64, 16, 1);
   tag("vertex-rate R1+64 stride 16, indexed 16 vo 100", P_VERT_DYN, 16);
   vkCmdDrawIndexed(cb, 16, 1, 0, 100, 0);
   /* per instance, divisor 1 */
   bind_all(r1, w1, 0, RBYTES, 16, 1);
   tag("instance-rate div 1 R1 stride 16, draw 2 x3 fi 7", P_INST1_DYN, 6);
   vkCmdDraw(cb, 2, 3, 0, 7);
   /* back to CS2's divisor 0 pipeline with a real buffer still bound */
   bind_all(r1, w1, 1024, 16, 0, 1);
   tag("pipeline switch back, R1+1024 stride 0", P_INST0_DYN, 4);
   vkCmdDraw(cb, 4, 1, 0, 0);

   /* static strides (DXVK passes no strides when they can't be dynamic) */
   bind_all(VK_NULL_HANDLE, NULL, 0, 0, 0, 0);
   tag("static strides: null b2", P_INST0_STATIC, 4);
   vkCmdDraw(cb, 4, 1, 0, 0);
   bind_all(r1, w1, 2048, 16, 0, 0);
   tag("static strides: R1+2048", P_INST0_STATIC, 4);
   vkCmdDraw(cb, 4, 1, 0, 0);
   bind_v1(VK_NULL_HANDLE, NULL, 0);
   tag("vkCmdBindVertexBuffers: null b2", P_INST0_STATIC, 4);
   vkCmdDraw(cb, 4, 1, 0, 0);
   bind_v1(r1, w1, 16);
   tag("vkCmdBindVertexBuffers: R1+16", P_INST0_STATIC, 4);
   vkCmdDraw(cb, 4, 1, 0, 0);
   bind_v1(r1, w1, 0);
   tag("vertex-rate static stride 16: R1", P_VERT16_STATIC, 8);
   vkCmdDraw(cb, 8, 1, 0, 0);
   bind_v1(VK_NULL_HANDLE, NULL, 0);
   tag("vertex-rate static stride 16: null", P_VERT16_STATIC, 8);
   vkCmdDraw(cb, 8, 1, 0, 0);
   bind_v1(r1, w1, 4096);
   tag("vertex-rate static stride 16: R1+4096", P_VERT16_STATIC, 8);
   vkCmdDraw(cb, 8, 1, 0, 0);

   /* null and real alternating per draw, as CS2's passes do */
   for (uint32_t i = 0; i < 8; i++) {
      char what[64];
      if (i & 1) {
         bind_all(r1, w1, 16 * (64 + i), 16, 0, 1);
         snprintf(what, sizeof(what), "alternate %u: R1+%u", i, 16 * (64 + i));
      } else {
         bind_all(VK_NULL_HANDLE, NULL, 0, 0, 0, 1);
         snprintf(what, sizeof(what), "alternate %u: null", i);
      }
      tag(what, P_INST0_DYN, 4);
      ind[1 + i] = (VkDrawIndexedIndirectCommand){ 4, 1, 0, 0, 1000 + i };
      vkCmdDrawIndexedIndirect(cb, ind_buf, (1 + i) * sizeof(*ind), 1, sizeof(*ind));
   }

   /* real b2 bound, then a meta blit outside the pass, then a draw without
    * rebinding */
   bind_all(r1, w1, 3072, 16, 0, 1);
   tag("R1+3072 before blit", P_INST0_DYN, 4);
   vkCmdDraw(cb, 4, 1, 0, 0);
   vkCmdEndRendering(cb);
   VkImageBlit blit = { { VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1 }, { { 0, 0, 0 }, { 16, 16, 1 } },
                        { VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1 }, { { 0, 0, 0 }, { 8, 8, 1 } } };
   vkCmdBlitImage(cb, img_a, VK_IMAGE_LAYOUT_GENERAL, img_b, VK_IMAGE_LAYOUT_GENERAL, 1, &blit,
                  VK_FILTER_LINEAR);
   VkMemoryBarrier mb = { VK_STRUCTURE_TYPE_MEMORY_BARRIER, .srcAccessMask = VK_ACCESS_MEMORY_WRITE_BIT,
      .dstAccessMask = VK_ACCESS_MEMORY_READ_BIT | VK_ACCESS_MEMORY_WRITE_BIT };
   vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT,
                        0, 1, &mb, 0, NULL, 0, NULL);
   vkCmdBeginRendering(cb, &ri);
   vkCmdBindDescriptorSets(cb, VK_PIPELINE_BIND_POINT_GRAPHICS, pl, 0, 1, &ds, 0, NULL);
   cur_pipe = -1;
   tag("after blit, R1+3072 still bound", P_INST0_DYN, 4);
   vkCmdDraw(cb, 4, 1, 0, 0);
   /* null b0 too: meta saves and restores binding 0 */
   {
      VkBuffer bufs[3] = { VK_NULL_HANDLE, inst_buf, VK_NULL_HANDLE };
      VkDeviceSize offs[3] = { 0, 0, 0 }, sizes[3] = { 0, 4096 * 4, 0 }, strides[3] = { 0, 4, 0 };
      vkCmdBindVertexBuffers2(cb, 0, 3, bufs, offs, sizes, strides);
   }
   vkCmdEndRendering(cb);
   vkCmdBlitImage(cb, img_a, VK_IMAGE_LAYOUT_GENERAL, img_b, VK_IMAGE_LAYOUT_GENERAL, 1, &blit,
                  VK_FILTER_LINEAR);
   vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT,
                        0, 1, &mb, 0, NULL, 0, NULL);
   vkCmdBeginRendering(cb, &ri);
   vkCmdBindDescriptorSets(cb, VK_PIPELINE_BIND_POINT_GRAPHICS, pl, 0, 1, &ds, 0, NULL);
   cur_pipe = -1;
   bind_all(r1, w1, 160, 16, 0, 1);
   tag("after null-b0 blit, everything rebound, R1+160", P_INST0_DYN, 4);
   vkCmdDraw(cb, 4, 1, 0, 0);
   vkCmdEndRendering(cb);
   CHECK(vkEndCommandBuffer(cb));

   /* ---- command buffer 2: starts with a real b2, then null, then real ---- */
   cb = cbs[1];
   cur_pipe = -1;
   CHECK(vkBeginCommandBuffer(cb, &cbbi));
   vkCmdBeginRendering(cb, &ri);
   vkCmdBindDescriptorSets(cb, VK_PIPELINE_BIND_POINT_GRAPHICS, pl, 0, 1, &ds, 0, NULL);
   bind_all(r1, w1, 160, 16, 0, 1);
   tag("cb2: R1+160 (same range as cb1's last)", P_INST0_DYN, 4);
   vkCmdDraw(cb, 4, 1, 0, 0);
   bind_all(VK_NULL_HANDLE, NULL, 0, 0, 0, 1);
   tag("cb2: null", P_INST0_DYN, 4);
   vkCmdDraw(cb, 4, 1, 0, 0);
   bind_all(r2b, w2, 800, 16, 0, 1);
   tag("cb2: R2+800", P_INST0_DYN, 4);
   vkCmdDraw(cb, 4, 1, 0, 0);
   vkCmdEndRendering(cb);
   VkMemoryBarrier host = { VK_STRUCTURE_TYPE_MEMORY_BARRIER, .srcAccessMask = VK_ACCESS_SHADER_WRITE_BIT,
      .dstAccessMask = VK_ACCESS_HOST_READ_BIT };
   vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT, VK_PIPELINE_STAGE_HOST_BIT,
                        0, 1, &host, 0, NULL, 0, NULL);
   CHECK(vkEndCommandBuffer(cb));

   VkFenceCreateInfo fci = { VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
   VkFence fence;
   CHECK(vkCreateFence(dev, &fci, NULL, &fence));
   VkSubmitInfo si[2] = {
      { VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cbs[0] },
      { VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cbs[1] },
   };
   CHECK(vkQueueSubmit(q, 1, &si[0], VK_NULL_HANDLE));
   CHECK(vkQueueSubmit(q, 1, &si[1], fence));
   CHECK(vkWaitForFences(dev, 1, &fence, VK_TRUE, 5ull * 1000 * 1000 * 1000));

   const uint32_t *o = m_out;
   uint32_t count = o[0], shown = 0, bad_total = 0, total_expected = 0;
   const uint32_t *rec = o + 4;
   for (uint32_t s = 0; s < count && s < MAXREC; s++) {
      const uint32_t *r = rec + s * 16;
      uint32_t tg = r[0], vi = r[1], ii = r[2], bi = r[3];
      const char *why = NULL;
      uint32_t e[4] = { 0 };
      if (tg >= ntags) {
         why = "bad tag";
      } else {
         struct tag_info *t = &tags[tg];
         int oob;
         t->seen++;
         expect_b2(&t->b2, vi, ii, bi, e, &oob);
         if (r[4] != vi) why = "b0 fetch != gl_VertexIndex";
         else if (r[5] != ii + 0x10000u) why = "b1 fetch != gl_InstanceIndex";
         else if (oob && (r[8] | r[9] | r[10] | r[12] | r[13] | r[14]) == 0 &&
                  (r[11] == 0 || r[11] == 1) && (r[15] == 0 || r[15] == 0x3f800000u)) why = NULL;
         else if (r[8] != e[0] || r[9] != e[1] || r[10] != e[2] || r[11] != e[3])
            why = t->b2.words ? "real b2 fetch wrong" : "null b2 fetch not zero";
         else if (r[12] != e[0] || r[13] != e[1] || r[14] != e[2] || r[15] != e[3])
            why = t->b2.words ? "real b2 RGBA32F fetch wrong" : "null b2 RGBA32F fetch not zero";
         if (why) t->bad++;
      }
      if (why) {
         bad_total++;
         if (shown++ < 32)
            printf("  BAD rec %u tag %u: %s (vi %u ii %u bi %u) got %08x %08x %08x %08x f %08x %08x %08x %08x want %08x %08x %08x %08x\n",
                   s, tg, why, vi, ii, bi, r[8], r[9], r[10], r[11], r[12], r[13], r[14], r[15],
                   e[0], e[1], e[2], e[3]);
      }
   }
   for (uint32_t t = 0; t < ntags; t++) {
      total_expected += tags[t].expected;
      int ok = tags[t].seen == tags[t].expected && !tags[t].bad;
      if (!ok) bad_total++;
      printf("tag %2u %-4s seen %4u/%-4u bad %4u  %s\n", t, ok ? "ok" : "FAIL", tags[t].seen,
             tags[t].expected, tags[t].bad, tags[t].what);
   }
   printf("RESULT %s records=%u expected=%u bad=%u\n", bad_total ? "FAIL" : "PASS", count,
          total_expected, bad_total);
   vkDeviceWaitIdle(dev);
   vkDestroyDevice(dev, NULL);
   vkDestroyInstance(inst, NULL);
   return bad_total ? 2 : 0;
}
