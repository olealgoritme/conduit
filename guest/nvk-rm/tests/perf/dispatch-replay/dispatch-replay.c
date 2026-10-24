/* dispatch-replay: run one captured compute dispatch or draw (NVK_DISPATCH_CAPTURE) on any Vulkan driver.
 *
 * Loads a capture directory (manifest.json, shader.spv, push.bin, r<N>.bin), creates every resource the
 * shader reads with the captured format, size and contents, builds the compute pipeline from the captured
 * SPIR-V with an equivalent pipeline layout (same sets, bindings, descriptor types, push constant block),
 * patches the root buffer addresses in the push constants, dispatches the captured grid and reports GPU
 * time per dispatch (timestamps; median of N) and a checksum of everything the shader may write.
 * Writable resources are restored to the captured contents before every dispatch (--no-reset: not), so
 * every dispatch, and the checksum, start from the captured state.
 *
 * A draw capture ("kind": "draw") is replayed as the same draw: a graphics pipeline from the captured
 * vertex and fragment shaders with the captured state, dynamic rendering into attachments with the
 * captured formats and contents (load and store), timed from before the render pass to after it.
 *
 *   dispatch-replay [options] CAPTURE_DIR
 *
 * Builds for Linux (gcc) and Windows (MinGW); see build.sh and README.md.
 */
#define _GNU_SOURCE
#ifdef _WIN32
#include <windows.h>
#include <direct.h>
#else
#include <dlfcn.h>
#include <sys/stat.h>
#include <time.h>
#endif
#define VK_NO_PROTOTYPES
#include <vulkan/vulkan.h>
#include <inttypes.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include "json.h"

/* ------------------------------------------------------------------------------------------- loader */
#define VKFUNCS(X) \
 X(vkDestroyInstance) X(vkEnumeratePhysicalDevices) X(vkGetPhysicalDeviceProperties) X(vkGetPhysicalDeviceFeatures2) \
 X(vkGetPhysicalDeviceProperties2) \
 X(vkGetPhysicalDeviceMemoryProperties) X(vkGetPhysicalDeviceQueueFamilyProperties) X(vkEnumerateDeviceExtensionProperties) \
 X(vkGetPhysicalDeviceFormatProperties) X(vkCreateDevice) X(vkDestroyDevice) X(vkGetDeviceQueue) \
 X(vkCreateDescriptorSetLayout) X(vkCreatePipelineLayout) X(vkCreateShaderModule) X(vkCreateComputePipelines) \
 X(vkCreateDescriptorPool) X(vkAllocateDescriptorSets) X(vkUpdateDescriptorSets) X(vkCreateBuffer) \
 X(vkGetBufferMemoryRequirements) X(vkAllocateMemory) X(vkBindBufferMemory) X(vkMapMemory) X(vkGetBufferDeviceAddress) \
 X(vkCreateBufferView) X(vkCreateImage) X(vkGetImageMemoryRequirements) X(vkBindImageMemory) X(vkCreateImageView) \
 X(vkCreateSampler) X(vkCreateCommandPool) X(vkAllocateCommandBuffers) X(vkBeginCommandBuffer) X(vkEndCommandBuffer) \
 X(vkResetCommandBuffer) X(vkCmdBindPipeline) X(vkCmdBindDescriptorSets) X(vkCmdPushConstants) X(vkCmdDispatchBase) \
 X(vkCmdPipelineBarrier) X(vkCmdCopyBuffer) X(vkCmdCopyBufferToImage) X(vkCmdCopyImageToBuffer) X(vkCmdFillBuffer) \
 X(vkCmdResetQueryPool) X(vkCmdWriteTimestamp) X(vkCreateQueryPool) X(vkGetQueryPoolResults) X(vkQueueSubmit) \
 X(vkCreateFence) X(vkWaitForFences) X(vkResetFences) X(vkDeviceWaitIdle) \
 X(vkCreatePipelineCache) X(vkGetPipelineCacheData) \
 X(vkCreateGraphicsPipelines) X(vkCmdBeginRendering) X(vkCmdEndRendering) X(vkCmdDraw) X(vkCmdSetViewport) \
 X(vkCmdSetScissor) X(vkCmdDrawIndexed) X(vkCmdBindIndexBuffer) X(vkCmdBindVertexBuffers)
#define X(n) static PFN_##n n;
VKFUNCS(X)
#undef X
static PFN_vkGetInstanceProcAddr gipa;

static void die(const char *fmt, ...)
{
    va_list ap; va_start(ap, fmt); fprintf(stderr, "dispatch-replay: "); vfprintf(stderr, fmt, ap); va_end(ap);
    fputc('\n', stderr); exit(1);
}
#define VK(x) do { VkResult r_ = (x); if (r_ != VK_SUCCESS) die("%s failed: %d (line %d)", #x, (int)r_, __LINE__); } while (0)

static void load_loader(void)
{
#ifdef _WIN32
    HMODULE m = LoadLibraryA("vulkan-1.dll");
    if (m) gipa = (PFN_vkGetInstanceProcAddr)(void *)GetProcAddress(m, "vkGetInstanceProcAddr");
#else
    void *m = dlopen("libvulkan.so.1", RTLD_NOW);
    if (!m) m = dlopen("libvulkan.so", RTLD_NOW);
    if (m) gipa = (PFN_vkGetInstanceProcAddr)dlsym(m, "vkGetInstanceProcAddr");
#endif
    if (!gipa) die("cannot load the Vulkan loader");
}

static double now_ms(void)
{
#ifdef _WIN32
    LARGE_INTEGER f, c; QueryPerformanceFrequency(&f); QueryPerformanceCounter(&c);
    return 1000.0 * (double)c.QuadPart / (double)f.QuadPart;
#else
    struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec * 1000.0 + t.tv_nsec / 1e6;
#endif
}

/* ------------------------------------------------------------------------------------------- files */
static char *g_dir;

static void *read_file(const char *name, uint64_t *size_out, int must)
{
    char path[1024]; snprintf(path, sizeof path, "%s/%s", g_dir, name);
    FILE *f = fopen(path, "rb");
    if (!f) { if (must) die("cannot open %s", path); return NULL; }
    fseek(f, 0, SEEK_END); long long n = ftell(f); fseek(f, 0, SEEK_SET);
    void *p = malloc(n + 1);
    if (!p || fread(p, 1, n, f) != (size_t)n) die("cannot read %s", path);
    ((char *)p)[n] = 0;
    fclose(f);
    if (size_out) *size_out = n;
    return p;
}

static uint64_t fnv64(const void *data, uint64_t n, uint64_t h)
{
    const uint8_t *b = data;
    for (uint64_t i = 0; i < n; i++) { h ^= b[i]; h *= 0x00000100000001b3ull; }
    return h;
}

/* ------------------------------------------------------------------------------------------- options */
static struct {
    int n, warm, reset, compile_only, list, robust, verbose;
    const char *dev, *dump, *csv, *label, *dump_cache;
    int grid_override; uint32_t grid[3];
} O = { 15, 5, 1, 0, 0, 1, 0, NULL, NULL, NULL, NULL, NULL, 0, { 0, 0, 0 } };

static void usage(void)
{
    fprintf(stderr,
        "usage: dispatch-replay [options] CAPTURE_DIR\n"
        "  -n N            timed dispatches (default 15; the median is reported)\n"
        "  -w N            warm-up dispatches before them (default 5)\n"
        "  --no-reset      do not restore writable resources before each dispatch\n"
        "  --dev STR       device whose name contains STR (default: first NVIDIA-vendor device)\n"
        "  --list          list the devices and exit\n"
        "  --dump DIR      write what the shader may write, after the last dispatch, as DIR/r<N>.out.bin\n"
        "  --csv FILE      append one result line (tab separated) to FILE\n"
        "  --label STR     a name for the run in the csv line (default: the device name)\n"
        "  --grid X,Y,Z    dispatch this grid instead of the captured one\n"
        "  --no-robust     do not enable robustBufferAccess / robustness2 (vkd3d-proton enables them)\n"
        "  --compile-only  create the pipeline and exit (works with drm-shim)\n"
        "  --dump-cache F  create the pipeline with an empty pipeline cache and write its data to F\n"
        "  -v              print every resource\n");
    exit(2);
}

/* ------------------------------------------------------------------------------------------- capture */
#define MAX_RES 256
#define MAX_SUB 16
#define MAX_BIND 64

struct sub { uint32_t mip, ext[3], layers; uint64_t off, size; };

enum rkind { R_NULL, R_IMAGE, R_TEXEL, R_BUFFER, R_SAMPLER, R_BDA, R_ERROR };

struct res {
    int id; enum rkind kind; int has_desc; uint32_t set, binding, index; VkDescriptorType dtype;
    int push_offset; int writable; const char *file, *note; uint64_t size;
    /* image */
    VkImageType itype; VkImageViewType vtype; VkFormat ifmt, vfmt, cfmt; VkImageAspectFlags aspect;
    uint32_t ext[3], base_mip, mips, base_layer, layers; VkComponentMapping swz; int nsub; struct sub sub[MAX_SUB];
    /* buffers */
    uint64_t range; VkFormat bfmt; uint64_t addr;
    /* sampler */
    const struct json *samp;
    /* Vulkan */
    VkImage img; VkImageView view; VkBuffer buf; VkBufferView bview; VkSampler sampler;
    VkBuffer stage; uint8_t *stage_map; uint64_t alloc_size, stage_size;
    VkBuffer pristine; /* device-local copy of the captured bytes of a writable resource, for resets */
    VkBuffer rb; uint8_t *rb_map;
    /* draws: an attachment (1 color, 2 depth, 3 stencil), its index and view format; or the index buffer (4), a
     * vertex buffer (5, attachment = its binding) */
    int role; uint32_t attachment; VkFormat att_format; VkImageAspectFlags att_aspects; int att_shared;
};

struct binding {
    uint32_t set, binding; int array; uint32_t count; int ntypes; VkDescriptorType types[8]; uint32_t nvk_stride;
};

static struct res R[MAX_RES]; static int NR;
static struct binding B[MAX_BIND]; static int NB;
static char g_hash[32] = "?";

static const char *kind_name[] = { "null", "image", "texel_buffer", "buffer", "sampler", "root_address", "error" };

static enum rkind parse_kind(const char *k)
{
    for (int i = 0; i < 7; i++) if (!strcmp(k, kind_name[i])) return (enum rkind)i;
    return R_ERROR;
}

static void add_binding_type(uint32_t set, uint32_t binding, int array, VkDescriptorType t, uint32_t stride)
{
    struct binding *b = NULL;
    for (int i = 0; i < NB; i++) if (B[i].set == set && B[i].binding == binding) b = &B[i];
    if (!b) {
        if (NB >= MAX_BIND) die("too many bindings");
        b = &B[NB++]; memset(b, 0, sizeof *b); b->set = set; b->binding = binding; b->count = 1;
    }
    b->array |= array;
    if (stride > b->nvk_stride) b->nvk_stride = stride;
    for (int i = 0; i < b->ntypes; i++) if (b->types[i] == t) return;
    if (b->ntypes < 8) b->types[b->ntypes++] = t;
}

static struct binding *find_binding(uint32_t set, uint32_t binding)
{
    for (int i = 0; i < NB; i++) if (B[i].set == set && B[i].binding == binding) return &B[i];
    return NULL;
}

static void load_manifest(struct json *m)
{
    const struct json *binds = jget(m, "bindings");
    for (int i = 0; binds && i < binds->n; i++) {
        const struct json *b = jidx(binds, i);
        const struct json *nl = jget(b, "nvk_layout");
        add_binding_type((uint32_t)jnum(b, "set", 0), (uint32_t)jnum(b, "binding", 0), jbool(b, "array", 0),
                         (VkDescriptorType)jnum(b, "descriptor_type", 0),
                         nl && jbool(nl, "have", 0) ? (uint32_t)jnum(nl, "stride", 0) : 0);
    }
    const struct json *rs = jget(m, "resources");
    for (int i = 0; rs && i < rs->n && NR < MAX_RES; i++) {
        const struct json *j = jidx(rs, i);
        struct res *r = &R[NR++]; memset(r, 0, sizeof *r);
        r->id = (int)jnum(j, "id", i);
        r->kind = parse_kind(jstr(j, "kind", "error"));
        r->has_desc = jget(j, "set") != NULL;
        r->set = (uint32_t)jnum(j, "set", 0); r->binding = (uint32_t)jnum(j, "binding", 0);
        r->index = (uint32_t)jnum(j, "index", 0); r->dtype = (VkDescriptorType)jnum(j, "descriptor_type", 0);
        r->push_offset = jget(j, "push_offset") ? (int)jnum(j, "push_offset", -1) : -1;
        r->writable = jbool(j, "writable", 0);
        r->file = jstr(j, "file", NULL); r->size = ju64(j, "size", 0); r->note = jstr(j, "note", NULL);
        {
            const char *role = jstr(j, "role", "");
            r->role = !strcmp(role, "color") ? 1 : !strcmp(role, "depth") ? 2 : !strcmp(role, "stencil") ? 3 :
                      !strcmp(role, "index") ? 4 : !strcmp(role, "vertex") ? 5 : 0;
            r->attachment = (uint32_t)jnum(j, "attachment", 0);
            r->att_format = (VkFormat)jnum(j, "attachment_format", 0);
        }
        if (r->kind == R_IMAGE) {
            r->itype = (VkImageType)jnum(j, "image_type", 1); r->vtype = (VkImageViewType)jnum(j, "view_type", 1);
            r->ifmt = (VkFormat)jnum(j, "image_format", 0); r->vfmt = (VkFormat)jnum(j, "view_format", 0);
            r->cfmt = (VkFormat)jnum(j, "copy_format", 0); r->aspect = (VkImageAspectFlags)jnum(j, "aspect", 1);
            const struct json *e = jget(j, "extent");
            for (int k = 0; k < 3; k++) r->ext[k] = (uint32_t)(jidx(e, k) ? jidx(e, k)->num : 1);
            r->base_mip = (uint32_t)jnum(j, "base_mip", 0); r->mips = (uint32_t)jnum(j, "mips", 1);
            r->base_layer = (uint32_t)jnum(j, "base_layer", 0); r->layers = (uint32_t)jnum(j, "layers", 1);
            const struct json *s = jget(j, "swizzle");
            r->swz.r = (VkComponentSwizzle)(jidx(s, 0) ? jidx(s, 0)->num : 0);
            r->swz.g = (VkComponentSwizzle)(jidx(s, 1) ? jidx(s, 1)->num : 0);
            r->swz.b = (VkComponentSwizzle)(jidx(s, 2) ? jidx(s, 2)->num : 0);
            r->swz.a = (VkComponentSwizzle)(jidx(s, 3) ? jidx(s, 3)->num : 0);
            const struct json *subs = jget(j, "subresources");
            for (int k = 0; subs && k < subs->n && k < MAX_SUB; k++) {
                const struct json *sj = jidx(subs, k); struct sub *sb = &r->sub[r->nsub++];
                sb->mip = (uint32_t)jnum(sj, "mip", 0); sb->layers = (uint32_t)jnum(sj, "layers", 1);
                const struct json *se = jget(sj, "extent");
                for (int q = 0; q < 3; q++) sb->ext[q] = (uint32_t)(jidx(se, q) ? jidx(se, q)->num : 1);
                sb->off = ju64(sj, "offset", 0); sb->size = ju64(sj, "size", 0);
            }
            if (jget(j, "sampler")) r->samp = jget(j, "sampler");
        } else if (r->kind == R_TEXEL || r->kind == R_BUFFER || r->kind == R_BDA) {
            r->range = ju64(j, "range", r->size); r->bfmt = (VkFormat)jnum(j, "format", 0);
            r->addr = ju64(j, "address", 0);
        } else if (r->kind == R_SAMPLER) {
            r->samp = jget(j, "sampler");
        }
        if (r->has_desc) {
            struct binding *b = find_binding(r->set, r->binding);
            if (!b) { add_binding_type(r->set, r->binding, 1, r->dtype, 0); b = find_binding(r->set, r->binding); }
            if (b->array && r->index + 1 > b->count) b->count = r->index + 1;
        }
    }
}

/* ------------------------------------------------------------------------------------------- Vulkan */
static VkInstance g_inst; static VkPhysicalDevice g_pd; static VkDevice g_dev; static VkQueue g_q; static uint32_t g_qf;
static VkPhysicalDeviceProperties g_pp; static VkPhysicalDeviceMemoryProperties g_mp;
static VkCommandPool g_cp; static VkFence g_fence;
static int g_have_nulldesc;

static uint32_t mem_type(uint32_t bits, VkMemoryPropertyFlags want, VkMemoryPropertyFlags avoid)
{
    for (uint32_t i = 0; i < g_mp.memoryTypeCount; i++)
        if ((bits & (1u << i)) && (g_mp.memoryTypes[i].propertyFlags & want) == want &&
            !(g_mp.memoryTypes[i].propertyFlags & avoid)) return i;
    for (uint32_t i = 0; i < g_mp.memoryTypeCount; i++)
        if ((bits & (1u << i)) && (g_mp.memoryTypes[i].propertyFlags & want) == want) return i;
    die("no memory type for bits 0x%x flags 0x%x", bits, want);
    return 0;
}

static VkDeviceMemory alloc_bind_buffer(VkBuffer buf, VkMemoryPropertyFlags want, int bda, void **map)
{
    VkMemoryRequirements mr; vkGetBufferMemoryRequirements(g_dev, buf, &mr);
    VkMemoryAllocateFlagsInfo fi = { VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_FLAGS_INFO, NULL, VK_MEMORY_ALLOCATE_DEVICE_ADDRESS_BIT, 0 };
    VkMemoryAllocateInfo ai = { VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, bda ? &fi : NULL, mr.size,
                                mem_type(mr.memoryTypeBits, want, (want & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT) ? 0 : VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT) };
    VkDeviceMemory m; VK(vkAllocateMemory(g_dev, &ai, NULL, &m)); VK(vkBindBufferMemory(g_dev, buf, m, 0));
    if (map) VK(vkMapMemory(g_dev, m, 0, VK_WHOLE_SIZE, 0, map));
    return m;
}

static VkBuffer make_buffer(uint64_t size, VkBufferUsageFlags usage, VkMemoryPropertyFlags want, void **map)
{
    if (size == 0) size = 16;
    VkBufferCreateInfo bi = { VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, NULL, 0, size, usage, VK_SHARING_MODE_EXCLUSIVE, 0, NULL };
    VkBuffer b; VK(vkCreateBuffer(g_dev, &bi, NULL, &b));
    alloc_bind_buffer(b, want, (usage & VK_BUFFER_USAGE_SHADER_DEVICE_ADDRESS_BIT) != 0, map);
    return b;
}

static VkCommandBuffer begin_cmd(void)
{
    VkCommandBufferAllocateInfo ai = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, NULL, g_cp, VK_COMMAND_BUFFER_LEVEL_PRIMARY, 1 };
    VkCommandBuffer cb; VK(vkAllocateCommandBuffers(g_dev, &ai, &cb));
    VkCommandBufferBeginInfo bi = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO, NULL, VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT, NULL };
    VK(vkBeginCommandBuffer(cb, &bi));
    return cb;
}

static void submit_wait(VkCommandBuffer cb)
{
    VK(vkEndCommandBuffer(cb));
    VkSubmitInfo si = { VK_STRUCTURE_TYPE_SUBMIT_INFO, NULL, 0, NULL, NULL, 1, &cb, 0, NULL };
    VK(vkResetFences(g_dev, 1, &g_fence));
    VK(vkQueueSubmit(g_q, 1, &si, g_fence));
    VK(vkWaitForFences(g_dev, 1, &g_fence, VK_TRUE, 60ull * 1000000000ull));
}

static void full_barrier(VkCommandBuffer cb)
{
    VkMemoryBarrier mb = { VK_STRUCTURE_TYPE_MEMORY_BARRIER, NULL, VK_ACCESS_MEMORY_WRITE_BIT,
                           VK_ACCESS_MEMORY_READ_BIT | VK_ACCESS_MEMORY_WRITE_BIT };
    vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT, 0, 1, &mb, 0, NULL, 0, NULL);
}

static int has_ext(VkExtensionProperties *ep, uint32_t ne, const char *name)
{
    for (uint32_t i = 0; i < ne; i++) if (!strcmp(ep[i].extensionName, name)) return 1;
    return 0;
}

/* Every feature of these structures the device has, except robustImageAccess (vkd3d-proton leaves it
 * off) and, with --no-robust, the robustness ones. */
static void create_device(void)
{
    load_loader();
    PFN_vkCreateInstance ci = (PFN_vkCreateInstance)gipa(NULL, "vkCreateInstance");
    VkApplicationInfo ai = { VK_STRUCTURE_TYPE_APPLICATION_INFO, NULL, "dispatch-replay", 1, NULL, 0, VK_API_VERSION_1_3 };
    VkInstanceCreateInfo ici = { VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, NULL, 0, &ai, 0, NULL, 0, NULL };
    VK(ci(&ici, NULL, &g_inst));
#define X(n) n = (PFN_##n)gipa(g_inst, #n);
    VKFUNCS(X)
#undef X
    uint32_t np = 16; VkPhysicalDevice pds[16]; VK(vkEnumeratePhysicalDevices(g_inst, &np, pds));
    for (uint32_t i = 0; i < np; i++) {
        VkPhysicalDeviceProperties pp; vkGetPhysicalDeviceProperties(pds[i], &pp);
        if (O.list) { printf("%u: %s (vendor 0x%04x device 0x%04x driver 0x%x)\n", i, pp.deviceName, pp.vendorID, pp.deviceID, pp.driverVersion); continue; }
        int ok = O.dev ? strstr(pp.deviceName, O.dev) != NULL : pp.vendorID == 0x10de;
        if (ok && !g_pd) { g_pd = pds[i]; g_pp = pp; }
    }
    if (O.list) exit(0);
    if (!g_pd) die("no matching device (--list shows them)");
    vkGetPhysicalDeviceMemoryProperties(g_pd, &g_mp);
    uint32_t nq = 0; vkGetPhysicalDeviceQueueFamilyProperties(g_pd, &nq, NULL);
    VkQueueFamilyProperties qfp[16]; if (nq > 16) nq = 16; vkGetPhysicalDeviceQueueFamilyProperties(g_pd, &nq, qfp);
    g_qf = ~0u;
    for (uint32_t i = 0; i < nq; i++) if ((qfp[i].queueFlags & VK_QUEUE_COMPUTE_BIT) && (qfp[i].queueFlags & VK_QUEUE_GRAPHICS_BIT)) { g_qf = i; break; }
    if (g_qf == ~0u) for (uint32_t i = 0; i < nq; i++) if (qfp[i].queueFlags & VK_QUEUE_COMPUTE_BIT) { g_qf = i; break; }
    if (g_qf == ~0u) die("no compute queue");
    if (!qfp[g_qf].timestampValidBits) die("the queue has no timestamps");

    uint32_t ne = 0; vkEnumerateDeviceExtensionProperties(g_pd, NULL, &ne, NULL);
    VkExtensionProperties *ep = calloc(ne + 1, sizeof *ep); vkEnumerateDeviceExtensionProperties(g_pd, NULL, &ne, ep);
    const char *exts[8]; uint32_t nexts = 0;
    int have_md = has_ext(ep, ne, "VK_EXT_mutable_descriptor_type"), have_vmd = has_ext(ep, ne, "VK_VALVE_mutable_descriptor_type");
    int have_fc2 = has_ext(ep, ne, "VK_KHR_shader_float_controls2"), have_rb2 = has_ext(ep, ne, "VK_EXT_robustness2");
    if (have_md) exts[nexts++] = "VK_EXT_mutable_descriptor_type"; else if (have_vmd) exts[nexts++] = "VK_VALVE_mutable_descriptor_type";
    if (have_fc2) exts[nexts++] = "VK_KHR_shader_float_controls2";
    if (have_rb2) exts[nexts++] = "VK_EXT_robustness2";
    int have_vad = has_ext(ep, ne, "VK_EXT_vertex_attribute_divisor"); /* draws with instance step rates */
    if (have_vad) exts[nexts++] = "VK_EXT_vertex_attribute_divisor";
    free(ep);

    struct { VkStructureType s; void *p; VkBool32 fc2; } fc2f = { (VkStructureType)1000528000, NULL, 0 };
    VkPhysicalDeviceRobustness2FeaturesEXT rb2 = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ROBUSTNESS_2_FEATURES_EXT, have_fc2 ? (void *)&fc2f : NULL };
    VkPhysicalDeviceMutableDescriptorTypeFeaturesEXT mdf = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_MUTABLE_DESCRIPTOR_TYPE_FEATURES_EXT,
                                                             have_rb2 ? (void *)&rb2 : (have_fc2 ? (void *)&fc2f : NULL) };
    VkPhysicalDeviceVulkan13Features f13 = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES, (have_md || have_vmd) ? (void *)&mdf : mdf.pNext };
    VkPhysicalDeviceVertexAttributeDivisorFeaturesEXT vadf = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VERTEX_ATTRIBUTE_DIVISOR_FEATURES_EXT,
                                                               f13.pNext };
    if (have_vad) f13.pNext = &vadf;
    VkPhysicalDeviceVulkan12Features f12 = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES, &f13 };
    VkPhysicalDeviceVulkan11Features f11 = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_1_FEATURES, &f12 };
    VkPhysicalDeviceFeatures2 f2 = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2, &f11 };
    vkGetPhysicalDeviceFeatures2(g_pd, &f2);
    if (!(have_md || have_vmd) || !mdf.mutableDescriptorType) die("the device has no mutable descriptor type");
    if (!f12.bufferDeviceAddress) die("the device has no buffer device address");
    f13.robustImageAccess = VK_FALSE;
    rb2.robustImageAccess2 = rb2.robustImageAccess2 && O.robust;
    rb2.robustBufferAccess2 = rb2.robustBufferAccess2 && O.robust;
    f2.features.robustBufferAccess = f2.features.robustBufferAccess && O.robust;
    g_have_nulldesc = have_rb2 && rb2.nullDescriptor;
    f12.bufferDeviceAddressCaptureReplay = VK_FALSE; f12.bufferDeviceAddressMultiDevice = VK_FALSE;

    float prio = 1; VkDeviceQueueCreateInfo qci = { VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, NULL, 0, g_qf, 1, &prio };
    VkDeviceCreateInfo dci = { VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, &f2, 0, 1, &qci, 0, NULL, nexts, exts, NULL };
    VK(vkCreateDevice(g_pd, &dci, NULL, &g_dev));
    vkGetDeviceQueue(g_dev, g_qf, 0, &g_q);
    VkCommandPoolCreateInfo cpi = { VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, NULL, VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT, g_qf };
    VK(vkCreateCommandPool(g_dev, &cpi, NULL, &g_cp));
    VkFenceCreateInfo fci = { VK_STRUCTURE_TYPE_FENCE_CREATE_INFO, NULL, 0 };
    VK(vkCreateFence(g_dev, &fci, NULL, &g_fence));
    fprintf(stderr, "device: %s (vendor 0x%04x, driver 0x%x), robustness %s, null descriptors %s\n", g_pp.deviceName,
            g_pp.vendorID, g_pp.driverVersion, O.robust ? "as vkd3d-proton" : "off", g_have_nulldesc ? "yes" : "no");
}

/* ------------------------------------------------------------------------------------------- resources */
static void copy_regions(const struct res *r, VkBufferImageCopy *rg)
{
    for (int k = 0; k < r->nsub; k++) {
        const struct sub *s = &r->sub[k];
        rg[k] = (VkBufferImageCopy){ s->off, 0, 0, { r->aspect, s->mip - r->base_mip, 0, s->layers }, { 0, 0, 0 },
                                     { s->ext[0], s->ext[1], s->ext[2] } };
    }
}

static void create_image(struct res *r)
{
    int storage = r->dtype == VK_DESCRIPTOR_TYPE_STORAGE_IMAGE;
    VkFormat fmt = r->vfmt;
    VkImageCreateFlags flags = 0;
    if (r->aspect != VK_IMAGE_ASPECT_COLOR_BIT) {
        /* A depth or stencil aspect: a single-aspect image of the copy format (D32S8 -> D32, D24S8 -> X8D24) */
        fmt = r->cfmt;
        if (fmt == VK_FORMAT_X8_D24_UNORM_PACK32) {
            VkFormatProperties fp; vkGetPhysicalDeviceFormatProperties(g_pd, fmt, &fp);
            if (!(fp.optimalTilingFeatures & VK_FORMAT_FEATURE_SAMPLED_IMAGE_BIT)) fmt = VK_FORMAT_D24_UNORM_S8_UINT;
        }
    } else {
        VkFormatProperties fp; vkGetPhysicalDeviceFormatProperties(g_pd, fmt, &fp);
        VkFormatFeatureFlags need = storage ? VK_FORMAT_FEATURE_STORAGE_IMAGE_BIT : VK_FORMAT_FEATURE_SAMPLED_IMAGE_BIT;
        if (!(fp.optimalTilingFeatures & need) && r->ifmt != fmt) {
            /* e.g. a storage view of an sRGB-typed resource: the image in its own format, a mutable view */
            fmt = r->ifmt; flags |= VK_IMAGE_CREATE_MUTABLE_FORMAT_BIT | VK_IMAGE_CREATE_EXTENDED_USAGE_BIT;
        }
    }
    if (r->vtype == VK_IMAGE_VIEW_TYPE_CUBE || r->vtype == VK_IMAGE_VIEW_TYPE_CUBE_ARRAY) flags |= VK_IMAGE_CREATE_CUBE_COMPATIBLE_BIT;
    VkImageUsageFlags usage = VK_IMAGE_USAGE_TRANSFER_DST_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT;
    usage |= storage ? VK_IMAGE_USAGE_STORAGE_BIT : VK_IMAGE_USAGE_SAMPLED_BIT;
    VkImageCreateInfo ic = { VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO, NULL, flags, r->itype, fmt,
                             { r->ext[0], r->ext[1], r->itype == VK_IMAGE_TYPE_3D ? r->ext[2] : 1 }, r->mips,
                             r->itype == VK_IMAGE_TYPE_3D ? 1 : r->layers, VK_SAMPLE_COUNT_1_BIT, VK_IMAGE_TILING_OPTIMAL,
                             usage, VK_SHARING_MODE_EXCLUSIVE, 0, NULL, VK_IMAGE_LAYOUT_UNDEFINED };
    VK(vkCreateImage(g_dev, &ic, NULL, &r->img));
    VkMemoryRequirements mr; vkGetImageMemoryRequirements(g_dev, r->img, &mr);
    VkMemoryAllocateInfo ai = { VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, NULL, mr.size,
                                mem_type(mr.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT) };
    VkDeviceMemory m; VK(vkAllocateMemory(g_dev, &ai, NULL, &m)); VK(vkBindImageMemory(g_dev, r->img, m, 0));
    VkImageViewUsageCreateInfo vu = { VK_STRUCTURE_TYPE_IMAGE_VIEW_USAGE_CREATE_INFO, NULL,
                                      storage ? VK_IMAGE_USAGE_STORAGE_BIT : VK_IMAGE_USAGE_SAMPLED_BIT };
    VkImageViewCreateInfo vc = { VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO, (flags & VK_IMAGE_CREATE_EXTENDED_USAGE_BIT) ? &vu : NULL, 0,
                                 r->img, r->vtype, r->aspect != VK_IMAGE_ASPECT_COLOR_BIT ? fmt : r->vfmt, r->swz,
                                 { r->aspect, 0, r->mips, 0, r->itype == VK_IMAGE_TYPE_3D ? 1 : r->layers } };
    VK(vkCreateImageView(g_dev, &vc, NULL, &r->view));
}

/* A color, depth or stencil attachment: an image of the captured image format (depth and stencil of one
 * depth/stencil image share it), a view of the attachment format */
static void create_attachment(struct res *r)
{
    if (r->role == 3 || r->role == 2) {
        for (int i = 0; i < NR; i++) {
            struct res *o = &R[i];
            if (o != r && (o->role == 2 || o->role == 3) && o->img && o->ifmt == r->ifmt) {
                r->img = o->img; r->view = o->view; r->att_aspects = o->att_aspects; r->att_shared = 1; return;
            }
        }
    }
    VkImageUsageFlags usage = VK_IMAGE_USAGE_TRANSFER_DST_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT |
        (r->role == 1 ? VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT : VK_IMAGE_USAGE_DEPTH_STENCIL_ATTACHMENT_BIT);
    VkImageCreateFlags flags = r->role == 1 && r->att_format != r->ifmt ? VK_IMAGE_CREATE_MUTABLE_FORMAT_BIT : 0;
    VkImageCreateInfo ic = { VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO, NULL, flags, VK_IMAGE_TYPE_2D, r->ifmt,
                             { r->ext[0], r->ext[1], 1 }, r->mips, r->layers, VK_SAMPLE_COUNT_1_BIT, VK_IMAGE_TILING_OPTIMAL,
                             usage, VK_SHARING_MODE_EXCLUSIVE, 0, NULL, VK_IMAGE_LAYOUT_UNDEFINED };
    VK(vkCreateImage(g_dev, &ic, NULL, &r->img));
    VkMemoryRequirements mr; vkGetImageMemoryRequirements(g_dev, r->img, &mr);
    VkMemoryAllocateInfo ai = { VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, NULL, mr.size,
                                mem_type(mr.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT) };
    VkDeviceMemory m; VK(vkAllocateMemory(g_dev, &ai, NULL, &m)); VK(vkBindImageMemory(g_dev, r->img, m, 0));
    VkImageAspectFlags aspects = r->role == 1 ? VK_IMAGE_ASPECT_COLOR_BIT : 0;
    if (r->role != 1)
        for (int i = 0; i < NR; i++)
            if ((R[i].role == 2 || R[i].role == 3) && R[i].ifmt == r->ifmt) aspects |= R[i].aspect;
    r->att_aspects = aspects;
    VkImageViewCreateInfo vc = { VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO, NULL, 0, r->img,
                                 r->layers > 1 ? VK_IMAGE_VIEW_TYPE_2D_ARRAY : VK_IMAGE_VIEW_TYPE_2D,
                                 r->role == 1 ? r->att_format : r->ifmt, { 0, 0, 0, 0 }, { aspects, 0, 1, 0, r->layers } };
    VK(vkCreateImageView(g_dev, &vc, NULL, &r->view));
}

static void create_sampler(struct res *r)
{
    const struct json *s = r->samp;
    const struct json *am = jget(s, "address_mode");
    VkSamplerReductionModeCreateInfo rci = { VK_STRUCTURE_TYPE_SAMPLER_REDUCTION_MODE_CREATE_INFO, NULL,
                                             (VkSamplerReductionMode)jnum(s, "reduction_mode", 0) };
    VkBorderColor bc = (VkBorderColor)jnum(s, "border_color", 0);
    if (bc > VK_BORDER_COLOR_INT_OPAQUE_WHITE) {
        const struct json *v = jget(s, "border_color_value");
        fprintf(stderr, "warning: r%03d: custom border color (%g %g %g %g as bits) replaced by transparent black\n", r->id,
                jidx(v, 0) ? jidx(v, 0)->num : 0, jidx(v, 1) ? jidx(v, 1)->num : 0, jidx(v, 2) ? jidx(v, 2)->num : 0,
                jidx(v, 3) ? jidx(v, 3)->num : 0);
        bc = VK_BORDER_COLOR_FLOAT_TRANSPARENT_BLACK;
    }
    VkSamplerCreateInfo sc = { VK_STRUCTURE_TYPE_SAMPLER_CREATE_INFO, rci.reductionMode ? &rci : NULL, 0,
        (VkFilter)jnum(s, "mag_filter", 0), (VkFilter)jnum(s, "min_filter", 0), (VkSamplerMipmapMode)jnum(s, "mipmap_mode", 0),
        (VkSamplerAddressMode)(jidx(am, 0) ? jidx(am, 0)->num : 0), (VkSamplerAddressMode)(jidx(am, 1) ? jidx(am, 1)->num : 0),
        (VkSamplerAddressMode)(jidx(am, 2) ? jidx(am, 2)->num : 0), (float)jnum(s, "mip_lod_bias", 0),
        (VkBool32)jnum(s, "anisotropy_enable", 0), (float)jnum(s, "max_anisotropy", 1), (VkBool32)jnum(s, "compare_enable", 0),
        (VkCompareOp)jnum(s, "compare_op", 0), (float)jnum(s, "min_lod", 0), (float)jnum(s, "max_lod", 1000), bc,
        (VkBool32)jnum(s, "unnormalized_coordinates", 0) };
    VK(vkCreateSampler(g_dev, &sc, NULL, &r->sampler));
}

/* Staging buffer with the captured bytes (also the source for resets) */
static void make_stage(struct res *r, uint64_t size)
{
    r->stage_size = size ? size : 16;
    r->stage = make_buffer(size, VK_BUFFER_USAGE_TRANSFER_SRC_BIT | VK_BUFFER_USAGE_TRANSFER_DST_BIT,
                           VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT, (void **)&r->stage_map);
    memset(r->stage_map, 0, size);
    if (r->file && r->size) {
        uint64_t n = 0; void *d = read_file(r->file, &n, 1);
        if (n != r->size) fprintf(stderr, "warning: %s has %" PRIu64 " bytes, manifest says %" PRIu64 "\n", r->file, n, r->size);
        memcpy(r->stage_map, d, n < size ? n : size);
        free(d);
    }
}

static uint64_t align_up(uint64_t v, uint64_t a) { return (v + a - 1) / a * a; }

static void create_resources(uint8_t *push)
{
    for (int i = 0; i < NR; i++) {
        struct res *r = &R[i];
        switch (r->kind) {
        case R_IMAGE:
            if (r->role) create_attachment(r); else create_image(r);
            make_stage(r, r->size);
            if (r->samp) create_sampler(r);
            break;
        case R_TEXEL: {
            r->alloc_size = align_up(r->range > r->size ? r->range : r->size, 256);
            r->buf = make_buffer(r->alloc_size, VK_BUFFER_USAGE_UNIFORM_TEXEL_BUFFER_BIT | VK_BUFFER_USAGE_STORAGE_TEXEL_BUFFER_BIT |
                                 VK_BUFFER_USAGE_TRANSFER_DST_BIT | VK_BUFFER_USAGE_TRANSFER_SRC_BIT, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT, NULL);
            make_stage(r, r->alloc_size);
            VkBufferViewCreateInfo bv = { VK_STRUCTURE_TYPE_BUFFER_VIEW_CREATE_INFO, NULL, 0, r->buf, r->bfmt, 0, r->range };
            VK(vkCreateBufferView(g_dev, &bv, NULL, &r->bview));
            break;
        }
        case R_BUFFER:
        case R_BDA: {
            uint64_t want = r->range > r->size ? r->range : r->size;
            r->alloc_size = align_up(want ? want : 16, 256);
            r->buf = make_buffer(r->alloc_size, VK_BUFFER_USAGE_STORAGE_BUFFER_BIT | VK_BUFFER_USAGE_UNIFORM_BUFFER_BIT |
                                 VK_BUFFER_USAGE_SHADER_DEVICE_ADDRESS_BIT | VK_BUFFER_USAGE_TRANSFER_DST_BIT |
                                 VK_BUFFER_USAGE_TRANSFER_SRC_BIT | VK_BUFFER_USAGE_INDEX_BUFFER_BIT |
                                 VK_BUFFER_USAGE_VERTEX_BUFFER_BIT, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT, NULL);
            make_stage(r, r->alloc_size);
            if (r->kind == R_BDA && r->push_offset >= 0 && r->push_offset + 8 <= 256) {
                VkBufferDeviceAddressInfo bi = { VK_STRUCTURE_TYPE_BUFFER_DEVICE_ADDRESS_INFO, NULL, r->buf };
                uint64_t va = vkGetBufferDeviceAddress(g_dev, &bi);
                memcpy(push + r->push_offset, &va, 8);
            }
            break;
        }
        case R_SAMPLER:
            if (r->samp) create_sampler(r);
            break;
        default:
            break;
        }
        if (O.verbose || r->kind == R_ERROR) {
            char where[64];
            if (r->has_desc) snprintf(where, sizeof where, "set %u binding %u index %u", r->set, r->binding, r->index);
            else snprintf(where, sizeof where, "push offset %d", r->push_offset);
            fprintf(stderr, "r%03d %-12s %s, %" PRIu64 " bytes%s%s%s\n", r->id, kind_name[r->kind], where, r->size,
                    r->writable ? ", writable" : "", r->note ? ": " : "", r->note ? r->note : "");
        }
    }
}

/* Copies of the captured contents into the resources: the upload (from the host staging buffers), and the
 * reset before each dispatch (from device-local copies, so that it does not cross PCIe) */
static void record_restore(VkCommandBuffer cb, int writable_only)
{
    for (int i = 0; i < NR; i++) {
        struct res *r = &R[i];
        if (writable_only && !r->writable) continue;
        VkBuffer src = writable_only && r->pristine ? r->pristine : r->stage;
        if (r->kind == R_IMAGE) {
            VkBufferImageCopy rg[MAX_SUB]; copy_regions(r, rg);
            if (r->nsub) vkCmdCopyBufferToImage(cb, src, r->img, VK_IMAGE_LAYOUT_GENERAL, (uint32_t)r->nsub, rg);
        } else if (r->kind == R_TEXEL || r->kind == R_BUFFER || r->kind == R_BDA) {
            VkBufferCopy c = { 0, 0, r->alloc_size };
            vkCmdCopyBuffer(cb, src, r->buf, 1, &c);
        }
    }
}

static void upload_all(void)
{
    VkCommandBuffer cb = begin_cmd();
    VkImageMemoryBarrier ib[MAX_RES]; uint32_t nib = 0;
    for (int i = 0; i < NR; i++) if (R[i].kind == R_IMAGE && !R[i].att_shared)
        ib[nib++] = (VkImageMemoryBarrier){ VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER, NULL, 0, VK_ACCESS_TRANSFER_WRITE_BIT,
            VK_IMAGE_LAYOUT_UNDEFINED, VK_IMAGE_LAYOUT_GENERAL, VK_QUEUE_FAMILY_IGNORED, VK_QUEUE_FAMILY_IGNORED, R[i].img,
            { R[i].role ? R[i].att_aspects : R[i].aspect, 0, VK_REMAINING_MIP_LEVELS, 0, VK_REMAINING_ARRAY_LAYERS } };
    if (nib) vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL, 0, NULL, nib, ib);
    record_restore(cb, 0);
    for (int i = 0; i < NR; i++) {
        struct res *r = &R[i];
        if (!r->writable || !r->stage) continue;
        r->pristine = make_buffer(r->stage_size, VK_BUFFER_USAGE_TRANSFER_SRC_BIT | VK_BUFFER_USAGE_TRANSFER_DST_BIT,
                                  VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT, NULL);
        VkBufferCopy c = { 0, 0, r->stage_size };
        vkCmdCopyBuffer(cb, r->stage, r->pristine, 1, &c);
    }
    full_barrier(cb);
    submit_wait(cb);
}

/* ------------------------------------------------------------------------------------------- pipeline */
static VkPipelineLayout g_pl; static VkPipeline g_pipe; static VkDescriptorSet g_sets[8]; static uint32_t g_nsets;
static double g_compile_ms;

/* Draw captures */
static int g_draw;
static VkShaderStageFlags g_stages = VK_SHADER_STAGE_COMPUTE_BIT;
static const struct json *g_dj; /* the manifest's "draw" object */
static uint32_t *g_vs; static uint64_t g_vs_size;

static double jarr(const struct json *o, const char *key, int i, double dflt)
{
    const struct json *a = jget(o, key);
    return a && jidx(a, i) ? jidx(a, i)->num : dflt;
}

/* The draw's pipeline: the captured vertex shader and fragment shader (fs), the captured fixed-function
 * state, dynamic rendering with the attachments' formats */
static void create_graphics_pipeline(VkShaderModule fs)
{
    const struct json *d = g_dj;
    VkShaderModuleCreateInfo vci = { VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO, NULL, 0, (size_t)g_vs_size, g_vs };
    VkShaderModule vs; VK(vkCreateShaderModule(g_dev, &vci, NULL, &vs));
    VkPipelineShaderStageCreateInfo st[2] = {
        { VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, NULL, 0, VK_SHADER_STAGE_VERTEX_BIT, vs, "main", NULL },
        { VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, NULL, 0, VK_SHADER_STAGE_FRAGMENT_BIT, fs, "main", NULL } };
    VkPipelineVertexInputStateCreateInfo vi = { VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO };
    VkVertexInputBindingDescription vib[32]; VkVertexInputAttributeDescription via[32];
    VkVertexInputBindingDivisorDescriptionEXT vid[32]; uint32_t nvid = 0;
    VkPipelineVertexInputDivisorStateCreateInfoEXT vdiv = { VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_DIVISOR_STATE_CREATE_INFO_EXT };
    {
        const struct json *vj = jget(d, "vertex_input"), *bs = jget(vj, "bindings"), *as = jget(vj, "attributes");
        for (int i = 0; bs && i < bs->n && i < 32; i++) {
            const struct json *b = jidx(bs, i);
            vib[i] = (VkVertexInputBindingDescription){ (uint32_t)jnum(b, "binding", 0), (uint32_t)jnum(b, "stride", 0),
                                                        (VkVertexInputRate)jnum(b, "input_rate", 0) };
            uint32_t div = (uint32_t)jnum(b, "divisor", 1);
            if (vib[i].inputRate == VK_VERTEX_INPUT_RATE_INSTANCE && div != 1)
                vid[nvid++] = (VkVertexInputBindingDivisorDescriptionEXT){ vib[i].binding, div };
            vi.vertexBindingDescriptionCount = (uint32_t)i + 1;
        }
        for (int i = 0; as && i < as->n && i < 32; i++) {
            const struct json *a = jidx(as, i);
            via[i] = (VkVertexInputAttributeDescription){ (uint32_t)jnum(a, "location", 0), (uint32_t)jnum(a, "binding", 0),
                                                          (VkFormat)jnum(a, "format", 0), (uint32_t)jnum(a, "offset", 0) };
            vi.vertexAttributeDescriptionCount = (uint32_t)i + 1;
        }
        vi.pVertexBindingDescriptions = vib; vi.pVertexAttributeDescriptions = via;
        if (nvid) { vdiv.vertexBindingDivisorCount = nvid; vdiv.pVertexBindingDivisors = vid; vi.pNext = &vdiv; }
    }
    VkPipelineInputAssemblyStateCreateInfo ia = { VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO, NULL, 0,
        (VkPrimitiveTopology)jnum(d, "topology", VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST), (VkBool32)jnum(d, "primitive_restart", 0) };
    VkPipelineViewportStateCreateInfo vp = { VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO, NULL, 0, 1, NULL, 1, NULL };
    VkPipelineRasterizationStateCreateInfo rs = { VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO, NULL, 0,
        (VkBool32)jnum(d, "depth_clamp", 0), (VkBool32)jnum(d, "rasterizer_discard", 0), (VkPolygonMode)jnum(d, "polygon_mode", 0),
        (VkCullModeFlags)jnum(d, "cull_mode", 0), (VkFrontFace)jnum(d, "front_face", 0), (VkBool32)jarr(d, "depth_bias", 0, 0),
        (float)jarr(d, "depth_bias", 1, 0), (float)jarr(d, "depth_bias", 3, 0), (float)jarr(d, "depth_bias", 2, 0), 1.0f };
    VkSampleMask smask = (VkSampleMask)jnum(d, "sample_mask", 0xffff) | 0xffff0000u;
    VkPipelineMultisampleStateCreateInfo ms = { VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO, NULL, 0,
        VK_SAMPLE_COUNT_1_BIT, VK_FALSE, 0, &smask, (VkBool32)jnum(d, "alpha_to_coverage", 0), VK_FALSE };
    const struct json *dj = jget(d, "depth"), *sj = jget(d, "stencil");
    VkStencilOpState so[2];
    for (int i = 0; i < 2; i++) {
        const struct json *f = jget(sj, i ? "back" : "front");
        so[i] = (VkStencilOpState){ (VkStencilOp)jnum(f, "fail", 0), (VkStencilOp)jnum(f, "pass", 0), (VkStencilOp)jnum(f, "depth_fail", 0),
                                    (VkCompareOp)jnum(f, "compare", 7), (uint32_t)jnum(f, "compare_mask", 0xff),
                                    (uint32_t)jnum(f, "write_mask", 0xff), (uint32_t)jnum(f, "reference", 0) };
    }
    VkFormat dfmt = VK_FORMAT_UNDEFINED, sfmt = VK_FORMAT_UNDEFINED;
    for (int i = 0; i < NR; i++) {
        if (R[i].role == 2 && R[i].kind == R_IMAGE) dfmt = R[i].ifmt;
        if (R[i].role == 3 && R[i].kind == R_IMAGE) sfmt = R[i].ifmt;
    }
    VkPipelineDepthStencilStateCreateInfo ds = { VK_STRUCTURE_TYPE_PIPELINE_DEPTH_STENCIL_STATE_CREATE_INFO, NULL, 0,
        (VkBool32)(dfmt && jnum(dj, "test", 0)), (VkBool32)(dfmt && jnum(dj, "write", 0)), (VkCompareOp)jnum(dj, "compare_op", 7),
        (VkBool32)(dfmt && jarr(dj, "bounds_test", 0, 0)), (VkBool32)(sfmt && jnum(sj, "test", 0)), so[0], so[1],
        (float)jarr(dj, "bounds_test", 1, 0), (float)jarr(dj, "bounds_test", 2, 1) };
    const struct json *bj = jget(d, "blend"), *ba = jget(bj, "attachments");
    uint32_t ncol = ba ? (uint32_t)ba->n : 0; if (ncol > 8) ncol = 8;
    VkPipelineColorBlendAttachmentState cba[8]; VkFormat cfmt[8];
    uint32_t cwe = (uint32_t)jnum(bj, "color_write_enables", 0xff);
    for (uint32_t i = 0; i < ncol; i++) {
        const struct json *a = jidx(ba, (int)i);
        cfmt[i] = (VkFormat)jnum(a, "format", 0);
        cba[i] = (VkPipelineColorBlendAttachmentState){ (VkBool32)jnum(a, "enable", 0), (VkBlendFactor)jnum(a, "src_color", 1),
            (VkBlendFactor)jnum(a, "dst_color", 0), (VkBlendOp)jnum(a, "color_op", 0), (VkBlendFactor)jnum(a, "src_alpha", 1),
            (VkBlendFactor)jnum(a, "dst_alpha", 0), (VkBlendOp)jnum(a, "alpha_op", 0),
            (cwe & (1u << i)) ? (VkColorComponentFlags)jnum(a, "write_mask", 0xf) : 0 };
        /* an attachment the pass has no image for is left out of rendering, not of the pipeline */
    }
    VkPipelineColorBlendStateCreateInfo cb = { VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO, NULL, 0,
        (VkBool32)jnum(bj, "logic_op_enable", 0), (VkLogicOp)jnum(bj, "logic_op", 0), ncol, cba,
        { (float)jarr(bj, "constants", 0, 0), (float)jarr(bj, "constants", 1, 0), (float)jarr(bj, "constants", 2, 0),
          (float)jarr(bj, "constants", 3, 0) } };
    VkDynamicState dyn[2] = { VK_DYNAMIC_STATE_VIEWPORT, VK_DYNAMIC_STATE_SCISSOR };
    VkPipelineDynamicStateCreateInfo dy = { VK_STRUCTURE_TYPE_PIPELINE_DYNAMIC_STATE_CREATE_INFO, NULL, 0, 2, dyn };
    VkPipelineRenderingCreateInfo ri = { VK_STRUCTURE_TYPE_PIPELINE_RENDERING_CREATE_INFO, NULL, 0, ncol, cfmt,
        dfmt ? (VkFormat)jnum(dj, "format", dfmt) : VK_FORMAT_UNDEFINED, sfmt ? (VkFormat)jnum(sj, "format", sfmt) : VK_FORMAT_UNDEFINED };
    if (ri.depthAttachmentFormat == VK_FORMAT_UNDEFINED && dfmt) ri.depthAttachmentFormat = dfmt;
    if (ri.stencilAttachmentFormat == VK_FORMAT_UNDEFINED && sfmt) ri.stencilAttachmentFormat = sfmt;
    VkGraphicsPipelineCreateInfo gci = { VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO, &ri, 0, 2, st, &vi, &ia, NULL, &vp, &rs,
                                         &ms, &ds, &cb, &dy, g_pl, VK_NULL_HANDLE, 0, VK_NULL_HANDLE, 0 };
    double t0 = now_ms();
    VK(vkCreateGraphicsPipelines(g_dev, VK_NULL_HANDLE, 1, &gci, NULL, &g_pipe));
    g_compile_ms = now_ms() - t0;
}

/* One replay of the draw: render pass (load, store) with the captured area, viewport and scissor */
static void record_draw(VkCommandBuffer cb)
{
    const struct json *d = g_dj;
    VkRenderingAttachmentInfo ca[8]; uint32_t ncol = 0;
    VkRenderingAttachmentInfo da = { VK_STRUCTURE_TYPE_RENDERING_ATTACHMENT_INFO }, sa = da;
    int have_d = 0, have_s = 0;
    const struct json *ba = jget(jget(d, "blend"), "attachments");
    ncol = ba ? (uint32_t)ba->n : 0; if (ncol > 8) ncol = 8;
    for (uint32_t i = 0; i < ncol; i++)
        ca[i] = (VkRenderingAttachmentInfo){ VK_STRUCTURE_TYPE_RENDERING_ATTACHMENT_INFO, NULL, VK_NULL_HANDLE,
            VK_IMAGE_LAYOUT_GENERAL, 0, VK_NULL_HANDLE, VK_IMAGE_LAYOUT_UNDEFINED, VK_ATTACHMENT_LOAD_OP_LOAD,
            VK_ATTACHMENT_STORE_OP_STORE, { { { 0 } } } };
    for (int i = 0; i < NR; i++) {
        struct res *r = &R[i];
        if (!r->role || !r->view) continue;
        VkRenderingAttachmentInfo *a = r->role == 1 ? (r->attachment < ncol ? &ca[r->attachment] : NULL) :
                                       r->role == 2 ? &da : &sa;
        if (!a) continue;
        a->imageView = r->view; a->imageLayout = VK_IMAGE_LAYOUT_GENERAL;
        a->loadOp = VK_ATTACHMENT_LOAD_OP_LOAD; a->storeOp = VK_ATTACHMENT_STORE_OP_STORE;
        if (r->role == 2) have_d = 1;
        if (r->role == 3) have_s = 1;
    }
    VkRenderingInfo ri = { VK_STRUCTURE_TYPE_RENDERING_INFO, NULL, 0,
        { { (int32_t)jarr(d, "render_area", 0, 0), (int32_t)jarr(d, "render_area", 1, 0) },
          { (uint32_t)jarr(d, "render_area", 2, 1), (uint32_t)jarr(d, "render_area", 3, 1) } },
        (uint32_t)jnum(d, "layers", 1) ? (uint32_t)jnum(d, "layers", 1) : 1, 0, ncol, ca,
        have_d ? &da : NULL, have_s ? &sa : NULL };
    vkCmdBeginRendering(cb, &ri);
    VkViewport v = { (float)jarr(d, "viewport", 0, 0), (float)jarr(d, "viewport", 1, 0), (float)jarr(d, "viewport", 2, 1),
                     (float)jarr(d, "viewport", 3, 1), (float)jarr(d, "viewport", 4, 0), (float)jarr(d, "viewport", 5, 1) };
    VkRect2D sc = { { (int32_t)jarr(d, "scissor", 0, 0), (int32_t)jarr(d, "scissor", 1, 0) },
                    { (uint32_t)jarr(d, "scissor", 2, 1), (uint32_t)jarr(d, "scissor", 3, 1) } };
    vkCmdSetViewport(cb, 0, 1, &v);
    vkCmdSetScissor(cb, 0, 1, &sc);
    for (int i = 0; i < NR; i++) {
        struct res *r = &R[i];
        if (r->role == 5 && r->buf) {
            VkDeviceSize o = 0;
            vkCmdBindVertexBuffers(cb, r->attachment, 1, &r->buf, &o);
        }
    }
    int indexed = (int)jnum(d, "indexed", 0);
    for (int i = 0; indexed && i < NR; i++)
        if (R[i].role == 4 && R[i].buf) vkCmdBindIndexBuffer(cb, R[i].buf, 0, (VkIndexType)jnum(d, "index_type", 0));
    if (indexed)
        /* the index resource starts at the draw's first index */
        vkCmdDrawIndexed(cb, (uint32_t)jnum(d, "index_count", 0), (uint32_t)jnum(d, "instance_count", 1), 0,
                         (int32_t)jnum(d, "vertex_offset", 0), (uint32_t)jnum(d, "first_instance", 0));
    else
        vkCmdDraw(cb, (uint32_t)jnum(d, "vertex_count", 3), (uint32_t)jnum(d, "instance_count", 1),
                  (uint32_t)jnum(d, "first_vertex", 0), (uint32_t)jnum(d, "first_instance", 0));
    vkCmdEndRendering(cb);
}

static void create_pipeline(const uint32_t *spv, uint64_t spv_size, uint32_t push_size)
{
    VkDescriptorSetLayout sl[8] = { 0 };
    g_nsets = 0;
    for (int k = 0; k < NB; k++) if (B[k].set + 1 > g_nsets) g_nsets = B[k].set + 1;
    if (g_nsets > 8) die("set %u: too many sets", g_nsets - 1);
    for (uint32_t s = 0; s < g_nsets; s++) {
        VkDescriptorSetLayoutBinding lb[MAX_BIND]; VkDescriptorBindingFlags fl[MAX_BIND]; VkMutableDescriptorTypeListEXT ml[MAX_BIND];
        static VkDescriptorType mtypes[MAX_BIND][9];
        uint32_t nl = 0; int uab = 0;
        for (int k = 0; k < NB; k++) {
            struct binding *b = &B[k];
            if (b->set != s) continue;
            /* vkd3d-proton's heaps are one mutable binding for several types; with a 16-byte NVK stride it
             * also holds storage buffers (raw SSBOs), which only changes the descriptor size */
            int mut = b->ntypes > 1;
            int nt = 0;
            for (int t = 0; t < b->ntypes; t++) mtypes[nl][nt++] = b->types[t];
            if (mut && b->nvk_stride >= 16) {
                int have = 0; for (int t = 0; t < nt; t++) have |= mtypes[nl][t] == VK_DESCRIPTOR_TYPE_STORAGE_BUFFER;
                if (!have) mtypes[nl][nt++] = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER;
            }
            lb[nl] = (VkDescriptorSetLayoutBinding){ b->binding, mut ? VK_DESCRIPTOR_TYPE_MUTABLE_EXT : b->types[0], b->count,
                                                     g_stages, NULL };
            ml[nl] = (VkMutableDescriptorTypeListEXT){ mut ? (uint32_t)nt : 0, mut ? mtypes[nl] : NULL };
            fl[nl] = VK_DESCRIPTOR_BINDING_PARTIALLY_BOUND_BIT;
            if (b->array && (mut || b->types[0] != VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER)) { fl[nl] |= VK_DESCRIPTOR_BINDING_UPDATE_AFTER_BIND_BIT; uab = 1; }
            nl++;
        }
        VkMutableDescriptorTypeCreateInfoEXT mci = { VK_STRUCTURE_TYPE_MUTABLE_DESCRIPTOR_TYPE_CREATE_INFO_EXT, NULL, nl, ml };
        VkDescriptorSetLayoutBindingFlagsCreateInfo bfi = { VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_BINDING_FLAGS_CREATE_INFO, &mci, nl, fl };
        VkDescriptorSetLayoutCreateInfo lci = { VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO, &bfi,
                                                uab ? VK_DESCRIPTOR_SET_LAYOUT_CREATE_UPDATE_AFTER_BIND_POOL_BIT : 0, nl, lb };
        VK(vkCreateDescriptorSetLayout(g_dev, &lci, NULL, &sl[s]));

        /* Pool and set */
        VkDescriptorPoolSize ps[MAX_BIND]; VkMutableDescriptorTypeListEXT pml[MAX_BIND]; uint32_t nps = 0;
        for (uint32_t q = 0; q < nl; q++) {
            ps[nps] = (VkDescriptorPoolSize){ lb[q].descriptorType, lb[q].descriptorCount };
            pml[nps] = ml[q];
            nps++;
        }
        VkMutableDescriptorTypeCreateInfoEXT pmci = { VK_STRUCTURE_TYPE_MUTABLE_DESCRIPTOR_TYPE_CREATE_INFO_EXT, NULL, nps, pml };
        VkDescriptorPoolCreateInfo pci = { VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO, &pmci,
                                           uab ? VK_DESCRIPTOR_POOL_CREATE_UPDATE_AFTER_BIND_BIT : 0, 1, nps ? nps : 1, nps ? ps : NULL };
        VkDescriptorPoolSize dummy = { VK_DESCRIPTOR_TYPE_SAMPLER, 1 };
        if (!nps) pci.pPoolSizes = &dummy;
        VkDescriptorPool pool; VK(vkCreateDescriptorPool(g_dev, &pci, NULL, &pool));
        VkDescriptorSetAllocateInfo dai = { VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO, NULL, pool, 1, &sl[s] };
        VK(vkAllocateDescriptorSets(g_dev, &dai, &g_sets[s]));
    }
    VkPushConstantRange pcr = { g_stages, 0, (push_size + 3) & ~3u };
    VkPipelineLayoutCreateInfo plci = { VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO, NULL, 0, g_nsets, sl, push_size ? 1 : 0, &pcr };
    VK(vkCreatePipelineLayout(g_dev, &plci, NULL, &g_pl));
    VkShaderModuleCreateInfo smci = { VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO, NULL, 0, (size_t)spv_size, spv };
    VkShaderModule sm; VK(vkCreateShaderModule(g_dev, &smci, NULL, &sm));
    if (g_draw) { create_graphics_pipeline(sm); return; }
    VkComputePipelineCreateInfo cpci = { VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO, NULL, 0,
        { VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, NULL, 0, VK_SHADER_STAGE_COMPUTE_BIT, sm, "main", NULL },
        g_pl, VK_NULL_HANDLE, 0 };
    VkPipelineCache cache = VK_NULL_HANDLE;
    if (O.dump_cache) {
        VkPipelineCacheCreateInfo pcci = { VK_STRUCTURE_TYPE_PIPELINE_CACHE_CREATE_INFO, NULL, 0, 0, NULL };
        VK(vkCreatePipelineCache(g_dev, &pcci, NULL, &cache));
    }
    double t0 = now_ms();
    VK(vkCreateComputePipelines(g_dev, cache, 1, &cpci, NULL, &g_pipe));
    g_compile_ms = now_ms() - t0;
    if (O.dump_cache) {
        size_t n = 0; VK(vkGetPipelineCacheData(g_dev, cache, &n, NULL));
        void *d = malloc(n ? n : 1); VK(vkGetPipelineCacheData(g_dev, cache, &n, d));
        FILE *f = fopen(O.dump_cache, "wb");
        if (!f || fwrite(d, 1, n, f) != n) die("cannot write %s", O.dump_cache);
        fclose(f); free(d);
        fprintf(stderr, "pipeline cache data: %zu bytes -> %s\n", n, O.dump_cache);
    }
}

static void write_descriptors(void)
{
    for (int i = 0; i < NR; i++) {
        struct res *r = &R[i];
        if (!r->has_desc) continue;
        struct binding *b = find_binding(r->set, r->binding);
        if (!b || r->set >= g_nsets || (b->array ? r->index >= b->count : r->index != 0)) continue;
        VkDescriptorImageInfo ii = { r->sampler, r->view, VK_IMAGE_LAYOUT_GENERAL };
        VkDescriptorBufferInfo bi = { r->buf, 0, r->range ? r->range : VK_WHOLE_SIZE };
        VkWriteDescriptorSet w = { VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, NULL, g_sets[r->set], r->binding, r->index, 1, r->dtype,
                                   NULL, NULL, NULL };
        int ok = 1;
        switch (r->dtype) {
        case VK_DESCRIPTOR_TYPE_SAMPLED_IMAGE: case VK_DESCRIPTOR_TYPE_STORAGE_IMAGE: case VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER:
            if (r->kind != R_IMAGE) { ok = g_have_nulldesc; ii.imageView = VK_NULL_HANDLE; }
            w.pImageInfo = &ii; break;
        case VK_DESCRIPTOR_TYPE_SAMPLER:
            if (r->kind != R_SAMPLER) ok = 0;
            w.pImageInfo = &ii; break;
        case VK_DESCRIPTOR_TYPE_UNIFORM_TEXEL_BUFFER: case VK_DESCRIPTOR_TYPE_STORAGE_TEXEL_BUFFER:
            if (r->kind != R_TEXEL) { ok = g_have_nulldesc; r->bview = VK_NULL_HANDLE; }
            w.pTexelBufferView = &r->bview; break;
        case VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER: case VK_DESCRIPTOR_TYPE_STORAGE_BUFFER:
            if (r->kind != R_BUFFER) { ok = g_have_nulldesc; bi.buffer = VK_NULL_HANDLE; bi.range = VK_WHOLE_SIZE; }
            w.pBufferInfo = &bi; break;
        default: ok = 0; break;
        }
        if (ok) vkUpdateDescriptorSets(g_dev, 1, &w, 0, NULL);
        else fprintf(stderr, "warning: r%03d (set %u binding %u index %u) left unwritten\n", r->id, r->set, r->binding, r->index);
    }
}

/* ------------------------------------------------------------------------------------------- run */
static int cmp_d(const void *a, const void *b) { double x = *(const double *)a, y = *(const double *)b; return x < y ? -1 : x > y; }

int main(int argc, char **argv)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    int i;
    for (i = 1; i < argc && argv[i][0] == '-'; i++) {
        const char *a = argv[i];
        if (!strcmp(a, "-n") && i + 1 < argc) O.n = atoi(argv[++i]);
        else if (!strcmp(a, "-w") && i + 1 < argc) O.warm = atoi(argv[++i]);
        else if (!strcmp(a, "--no-reset")) O.reset = 0;
        else if (!strcmp(a, "--dev") && i + 1 < argc) O.dev = argv[++i];
        else if (!strcmp(a, "--list")) O.list = 1;
        else if (!strcmp(a, "--dump") && i + 1 < argc) O.dump = argv[++i];
        else if (!strcmp(a, "--csv") && i + 1 < argc) O.csv = argv[++i];
        else if (!strcmp(a, "--label") && i + 1 < argc) O.label = argv[++i];
        else if (!strcmp(a, "--grid") && i + 1 < argc) { O.grid_override = sscanf(argv[++i], "%u,%u,%u", &O.grid[0], &O.grid[1], &O.grid[2]) == 3; }
        else if (!strcmp(a, "--no-robust")) O.robust = 0;
        else if (!strcmp(a, "--compile-only")) O.compile_only = 1;
        else if (!strcmp(a, "--dump-cache") && i + 1 < argc) O.dump_cache = argv[++i];
        else if (!strcmp(a, "-v")) O.verbose = 1;
        else usage();
    }
    if (O.list) { create_device(); return 0; }
    if (i != argc - 1) usage();
    g_dir = argv[i];
    if (O.n < 1) O.n = 1;
    if (O.warm < 0) O.warm = 0;

    char *text = read_file("manifest.json", NULL, 1);
    struct json *m = json_parse(text);
    if (!m) die("cannot parse %s/manifest.json", g_dir);
    if (strcmp(jstr(m, "format", ""), "nvk-dispatch-capture")) die("not an nvk-dispatch-capture manifest");
    snprintf(g_hash, sizeof g_hash, "%s", jstr(m, "src_hash", "?"));
    uint64_t spv_size = 0; uint32_t *spv = read_file(jstr(m, "spirv", "shader.spv"), &spv_size, 1);
    uint8_t push[256] = { 0 };
    { uint64_t n = 0; uint8_t *p = read_file(jstr(m, "push", "push.bin"), &n, 1); memcpy(push, p, n < 256 ? n : 256); free(p); }
    uint32_t push_size = (uint32_t)jnum(m, "push_size", 0);
    uint32_t grid[3], local[3];
    for (int k = 0; k < 3; k++) {
        grid[k] = (uint32_t)(jidx(jget(m, "grid"), k) ? jidx(jget(m, "grid"), k)->num : 1);
        local[k] = (uint32_t)(jidx(jget(m, "local_size"), k) ? jidx(jget(m, "local_size"), k)->num : 1);
    }
    uint32_t base[3];
    for (int k = 0; k < 3; k++) base[k] = (uint32_t)(jidx(jget(m, "base_group"), k) ? jidx(jget(m, "base_group"), k)->num : 0);
    if (O.grid_override) memcpy(grid, O.grid, sizeof grid);
    if (!strcmp(jstr(m, "kind", "dispatch"), "draw")) {
        g_draw = 1; g_stages = VK_SHADER_STAGE_ALL_GRAPHICS; g_dj = jget(m, "draw");
        if (!g_dj) die("draw capture without a draw section");
        g_vs = read_file(jstr(m, "vs_spirv", "vs.spv"), &g_vs_size, 1);
    }
    const char *unsup = jstr(m, "unsupported", "");
    if (unsup[0]) fprintf(stderr, "warning: the capture lists unsupported accesses: %s\n", unsup);
    load_manifest(m);

    create_device();
    create_pipeline(spv, spv_size, push_size);
    fprintf(stderr, "%s: pipeline created in %.1f ms\n", g_hash, g_compile_ms);
    if (O.compile_only) { printf("%s\tcompiled\t%.1f ms\n", g_hash, g_compile_ms); return 0; }

    create_resources(push);
    upload_all();
    write_descriptors();

    int nwrite = 0, nerr = 0; uint64_t bytes = 0;
    for (int k = 0; k < NR; k++) { nwrite += R[k].writable && R[k].kind != R_ERROR; nerr += R[k].kind == R_ERROR; bytes += R[k].size; }

    /* Dispatches: [reset, barrier] ts dispatch ts barrier */
    const uint32_t total = (uint32_t)(O.warm + O.n);
    VkQueryPoolCreateInfo qpi = { VK_STRUCTURE_TYPE_QUERY_POOL_CREATE_INFO, NULL, 0, VK_QUERY_TYPE_TIMESTAMP, 2 * total, 0 };
    VkQueryPool qp; VK(vkCreateQueryPool(g_dev, &qpi, NULL, &qp));
    VkCommandBuffer cb = begin_cmd();
    vkCmdResetQueryPool(cb, qp, 0, 2 * total);
    const VkPipelineBindPoint bp = g_draw ? VK_PIPELINE_BIND_POINT_GRAPHICS : VK_PIPELINE_BIND_POINT_COMPUTE;
    vkCmdBindPipeline(cb, bp, g_pipe);
    for (uint32_t s = 0; s < g_nsets; s++) vkCmdBindDescriptorSets(cb, bp, g_pl, s, 1, &g_sets[s], 0, NULL);
    if (push_size) vkCmdPushConstants(cb, g_pl, g_stages, 0, (push_size + 3) & ~3u, push);
    for (uint32_t it = 0; it < total; it++) {
        if (O.reset) { full_barrier(cb); record_restore(cb, 1); }
        full_barrier(cb);
        vkCmdWriteTimestamp(cb, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, qp, 2 * it);
        if (g_draw) record_draw(cb);
        else vkCmdDispatchBase(cb, base[0], base[1], base[2], grid[0], grid[1], grid[2]);
        vkCmdWriteTimestamp(cb, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, qp, 2 * it + 1);
    }
    full_barrier(cb);
    /* Read back what the shader may write */
    for (int k = 0; k < NR; k++) {
        struct res *r = &R[k];
        if (!r->writable || r->kind == R_ERROR || r->kind == R_NULL || !r->size) continue;
        r->rb = make_buffer(r->kind == R_IMAGE ? r->size : r->alloc_size, VK_BUFFER_USAGE_TRANSFER_DST_BIT,
                            VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT, (void **)&r->rb_map);
        if (r->kind == R_IMAGE) {
            VkBufferImageCopy rg[MAX_SUB]; copy_regions(r, rg);
            vkCmdCopyImageToBuffer(cb, r->img, VK_IMAGE_LAYOUT_GENERAL, r->rb, (uint32_t)r->nsub, rg);
        } else {
            VkBufferCopy c = { 0, 0, r->alloc_size }; vkCmdCopyBuffer(cb, r->buf, r->rb, 1, &c);
        }
    }
    full_barrier(cb);
    double t0 = now_ms();
    submit_wait(cb);
    double wall = now_ms() - t0;

    uint64_t ts[2 * 4096]; if (total > 4096) die("too many dispatches");
    VK(vkGetQueryPoolResults(g_dev, qp, 0, 2 * total, sizeof(uint64_t) * 2 * total, ts, sizeof(uint64_t),
                             VK_QUERY_RESULT_64_BIT | VK_QUERY_RESULT_WAIT_BIT));
    double *us = calloc(O.n, sizeof *us), sum = 0;
    for (int k = 0; k < O.n; k++) {
        uint32_t it = (uint32_t)(O.warm + k);
        us[k] = (double)(ts[2 * it + 1] - ts[2 * it]) * g_pp.limits.timestampPeriod / 1000.0;
        sum += us[k];
    }
    double cold = O.warm > 0 ? (double)(ts[1] - ts[0]) * g_pp.limits.timestampPeriod / 1000.0 : us[0];
    qsort(us, O.n, sizeof *us, cmp_d);
    double med = O.n % 2 ? us[O.n / 2] : 0.5 * (us[O.n / 2 - 1] + us[O.n / 2]);

    /* Checksums of what the shader may write (after the last dispatch, from the captured state with resets) */
    uint64_t all = 0xcbf29ce484222325ull;
    if (g_draw)
        printf("%s on %s: draw of %u %s x %u instances, area %ux%u, %d resources (%d writable, %d not captured), "
               "%.1f MiB, pipeline %.1f ms\n", g_hash, g_pp.deviceName, (uint32_t)jnum(g_dj, "vertex_count", 0),
               jnum(g_dj, "indexed", 0) ? "indices" : "vertices",
               (uint32_t)jnum(g_dj, "instance_count", 0), (uint32_t)jarr(g_dj, "render_area", 2, 0),
               (uint32_t)jarr(g_dj, "render_area", 3, 0), NR, nwrite, nerr, bytes / 1048576.0, g_compile_ms);
    else
    printf("%s on %s: grid %ux%ux%u local %ux%ux%u, %d resources (%d writable, %d not captured), %.1f MiB, pipeline %.1f ms\n",
           g_hash, g_pp.deviceName, grid[0], grid[1], grid[2], local[0], local[1], local[2], NR, nwrite, nerr,
           bytes / 1048576.0, g_compile_ms);
    printf("gpu_us median %.2f min %.2f mean %.2f max %.2f first %.2f (n=%d, warm-up %d, %s) wall %.1f ms\n", med, us[0],
           sum / O.n, us[O.n - 1], cold, O.n, O.warm, O.reset ? "writable resources restored before each" : "no reset", wall);
#ifdef _WIN32
    if (O.dump) _mkdir(O.dump);
#else
    if (O.dump) mkdir(O.dump, 0755);
#endif
    for (int k = 0; k < NR; k++) {
        struct res *r = &R[k];
        if (!r->rb_map) continue;
        uint64_t n = r->kind == R_IMAGE ? r->size : r->size; /* the captured bytes (buffers: the captured range) */
        uint64_t h = fnv64(r->rb_map, n, 0xcbf29ce484222325ull);
        all = fnv64(&h, sizeof h, all);
        if (r->kind == R_IMAGE && r->role)
            printf("checksum r%03d %016" PRIx64 "  %s attachment %u format %d %ux%u\n", r->id, h,
                   r->role == 1 ? "color" : r->role == 2 ? "depth" : "stencil", r->attachment, (int)r->att_format, r->ext[0], r->ext[1]);
        else if (r->kind == R_IMAGE)
            printf("checksum r%03d %016" PRIx64 "  %s set %u binding %u [%u] format %d %ux%ux%u mips %u layers %u\n", r->id, h,
                   kind_name[r->kind], r->set, r->binding, r->index, (int)r->vfmt, r->ext[0], r->ext[1], r->ext[2], r->mips, r->layers);
        else
            printf("checksum r%03d %016" PRIx64 "  %s %s%u bytes\n", r->id, h, kind_name[r->kind],
                   r->push_offset >= 0 && !r->has_desc ? "push " : "", (unsigned)n);
        if (O.dump) {
            char path[1024]; snprintf(path, sizeof path, "%s/r%03d.out.bin", O.dump, r->id);
            FILE *f = fopen(path, "wb"); if (f) { fwrite(r->rb_map, 1, n, f); fclose(f); }
        }
    }
    printf("checksum all %016" PRIx64 "\n", all);
    if (O.csv) {
        FILE *f = fopen(O.csv, "a");
        if (f) {
            char gs[48];
            if (g_draw) snprintf(gs, sizeof gs, "draw.%ux%u", (uint32_t)jarr(g_dj, "render_area", 2, 0),
                                 (uint32_t)jarr(g_dj, "render_area", 3, 0));
            else snprintf(gs, sizeof gs, "%u.%u.%u", grid[0], grid[1], grid[2]);
            fprintf(f, "%s\t%s\t%s\t%.2f\t%.2f\t%.2f\t%.2f\t%d\t%s\t%016" PRIx64 "\t%.1f\n", g_hash,
                    O.label ? O.label : g_pp.deviceName, gs, med, us[0], sum / O.n, us[O.n - 1], O.n,
                    O.reset ? "reset" : "noreset", all, g_compile_ms);
            fclose(f);
        }
    }
    vkDeviceWaitIdle(g_dev);
    return 0;
}
