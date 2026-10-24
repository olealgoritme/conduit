// dispcost: GPU time per compute dispatch, with and without the barriers a
// D3D12 game on vkd3d-proton puts between dependent dispatches.
//
// One command buffer holds N dependent dispatches (each invocation reads and
// writes its own word of one buffer) and a timestamp only before the first
// and after the last, so no per-dispatch timing perturbs the stream.  The
// result is (T(N) - T(empty)) / N, the median over R submits.  Columns are
// the barrier recorded between consecutive dispatches:
//
//   none      no barrier (dispatches may overlap)
//   mem1      vkCmdPipelineBarrier COMPUTE->COMPUTE, VkMemoryBarrier
//             SHADER_WRITE -> SHADER_READ
//   buf1      the same with a VkBufferMemoryBarrier on the data buffer
//   mem2      vkCmdPipelineBarrier2 COMPUTE->COMPUTE, VkMemoryBarrier2
//             SHADER_STORAGE_WRITE -> SHADER_STORAGE_READ
//   vkd3d     vkCmdPipelineBarrier2 as vkd3d-proton records a D3D12 UAV
//             barrier on a graphics queue: stages PRE_RASTERIZATION_SHADERS |
//             FRAGMENT_SHADER | COMPUTE_SHADER | CLEAR, access SHADER_READ |
//             SHADER_WRITE | TRANSFER_WRITE on both sides
//   full      vkCmdPipelineBarrier2 ALL_COMMANDS MEMORY_WRITE ->
//             ALL_COMMANDS MEMORY_READ | MEMORY_WRITE
//   vkd3d+pc  vkd3d, plus vkCmdPushConstants before every dispatch (root
//             constants that change between dispatches)
//   ts        no barrier, a BOTTOM_OF_PIPE timestamp after every dispatch:
//             what a per-dispatch GPU profiler (NVK_DISPATCH_PROFILE) adds;
//             it serializes the dispatches on both drivers
//   cpu       CPU microseconds per dispatch to record the vkd3d+pc stream
//
// Rows: shader (tiny ~10, mid ~100, big ~450 instructions), grid (workgroups
// of 64 invocations) and direct or indirect dispatch.
//
//   dispcost [-g GPU_NAME_SUBSTRING] [-n DISPATCHES] [-r SUBMITS]
//            [-q gfx|compute] [-f ROW_FILTER]
//
// Windows: VK_DIRECT_DRIVER=path\to\vulkan_nouveau.dll hands the loader that ICD
// as the only driver (../../tests/vk_direct_driver.h; the loader ignores
// VK_DRIVER_FILES in an elevated process).  Exit status 0 on success, 2 on a
// setup error.
#ifdef _WIN32
#define VK_NO_PROTOTYPES
#include <windows.h>
#else
#include <time.h>
#endif
#include <vulkan/vulkan.h>
#include <ctype.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifdef _WIN32
#include "vk_direct_driver.h"
#define FNS(X) X(vkCreateInstance) X(vkEnumeratePhysicalDevices) X(vkGetPhysicalDeviceProperties) \
  X(vkGetPhysicalDeviceProperties2) X(vkGetPhysicalDeviceMemoryProperties) \
  X(vkGetPhysicalDeviceQueueFamilyProperties) X(vkGetPhysicalDeviceFeatures2) \
  X(vkEnumerateDeviceExtensionProperties) X(vkCreateDevice) X(vkGetDeviceQueue) X(vkGetDeviceProcAddr) \
  X(vkCreateBuffer) X(vkGetBufferMemoryRequirements) X(vkAllocateMemory) X(vkBindBufferMemory) \
  X(vkMapMemory) X(vkCreateDescriptorSetLayout) X(vkCreatePipelineLayout) X(vkCreateShaderModule) \
  X(vkCreateComputePipelines) X(vkCreateDescriptorPool) X(vkAllocateDescriptorSets) \
  X(vkUpdateDescriptorSets) X(vkCreateCommandPool) X(vkAllocateCommandBuffers) \
  X(vkBeginCommandBuffer) X(vkCmdBindPipeline) X(vkCmdBindDescriptorSets) X(vkCmdPushConstants) \
  X(vkCmdDispatch) X(vkCmdDispatchIndirect) X(vkEndCommandBuffer) X(vkQueueSubmit) \
  X(vkQueueWaitIdle) X(vkResetCommandBuffer) X(vkCmdFillBuffer) X(vkCmdPipelineBarrier) \
  X(vkCmdResetQueryPool) X(vkCmdWriteTimestamp) X(vkCreateQueryPool) X(vkGetQueryPoolResults) \
  X(vkDestroyDevice) X(vkDestroyInstance) X(vkGetInstanceProcAddr)
#define DECL(n) static PFN_##n n;
FNS(DECL)
static int load_vk(void)
{
    HMODULE m = LoadLibraryA("vulkan-1.dll");
    if (!m) return 0;
    vkGetInstanceProcAddr = (PFN_vkGetInstanceProcAddr)GetProcAddress(m, "vkGetInstanceProcAddr");
    vkCreateInstance = (PFN_vkCreateInstance)vkGetInstanceProcAddr(NULL, "vkCreateInstance");
    return vkCreateInstance != NULL;
}
static void load_inst(VkInstance i)
{
#define LOADI(n) if (!n) n = (PFN_##n)vkGetInstanceProcAddr(i, #n);
    FNS(LOADI)
}
static double now_us(void)
{
    LARGE_INTEGER f, c;
    QueryPerformanceFrequency(&f); QueryPerformanceCounter(&c);
    return (double)c.QuadPart * 1e6 / (double)f.QuadPart;
}
#else
#define direct_driver_chain(ici) ((void)(ici))
static int load_vk(void) { return 1; }
static void load_inst(VkInstance i) { (void)i; }
static double now_us(void)
{
    struct timespec ts; clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1e6 + ts.tv_nsec / 1e3;
}
#endif

#include "dispcost_spv.h"

#define CHK(x) do { VkResult r_ = (x); if (r_ != VK_SUCCESS) { fprintf(stderr, "%s failed: %d\n", #x, r_); exit(2); } } while (0)

static PFN_vkCmdPipelineBarrier2 pCmdPipelineBarrier2;

enum bar { B_NONE, B_MEM1, B_BUF1, B_MEM2, B_VKD3D, B_FULL, B_VKD3D_PC, B_TS, B_COUNT };
static const char *bar_name[B_COUNT] = { "none", "mem1", "buf1", "mem2", "vkd3d", "full", "vkd3d+pc", "ts" };

static int stristr(const char *h, const char *n)
{
    size_t ln = strlen(n);
    for (; *h; h++) {
        size_t k = 0;
        while (k < ln && h[k] && tolower((unsigned char)h[k]) == tolower((unsigned char)n[k])) k++;
        if (k == ln) return 1;
    }
    return ln == 0;
}

static uint32_t mem_type(const VkPhysicalDeviceMemoryProperties *mp, uint32_t bits, VkMemoryPropertyFlags want)
{
    for (uint32_t i = 0; i < mp->memoryTypeCount; i++)
        if ((bits & (1u << i)) && (mp->memoryTypes[i].propertyFlags & want) == want) return i;
    for (uint32_t i = 0; i < mp->memoryTypeCount; i++)
        if (bits & (1u << i)) return i;
    fprintf(stderr, "no memory type\n"); exit(2);
}

static int cmp_d(const void *a, const void *b)
{
    double x = *(const double *)a, y = *(const double *)b;
    return x < y ? -1 : x > y;
}

struct ctx {
    VkDevice dev; VkQueue q; VkCommandBuffer cb; VkQueryPool qp;
    VkPipelineLayout pl; VkDescriptorSet ds; VkBuffer data, args;
    double ts_ns; uint64_t ts_mask; int reps;
};

static void emit_barrier(const struct ctx *c, enum bar b)
{
    const VkPipelineStageFlags2 vkd3d_stages = VK_PIPELINE_STAGE_2_PRE_RASTERIZATION_SHADERS_BIT |
        VK_PIPELINE_STAGE_2_FRAGMENT_SHADER_BIT | VK_PIPELINE_STAGE_2_COMPUTE_SHADER_BIT | VK_PIPELINE_STAGE_2_CLEAR_BIT;
    const VkAccessFlags2 vkd3d_access = VK_ACCESS_2_SHADER_READ_BIT | VK_ACCESS_2_SHADER_WRITE_BIT |
        VK_ACCESS_2_TRANSFER_WRITE_BIT;
    switch (b) {
    case B_NONE:
        break;
    case B_MEM1: {
        VkMemoryBarrier mb = { VK_STRUCTURE_TYPE_MEMORY_BARRIER, NULL, VK_ACCESS_SHADER_WRITE_BIT, VK_ACCESS_SHADER_READ_BIT };
        vkCmdPipelineBarrier(c->cb, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, 0,
                             1, &mb, 0, NULL, 0, NULL);
        break;
    }
    case B_BUF1: {
        VkBufferMemoryBarrier bb = { VK_STRUCTURE_TYPE_BUFFER_MEMORY_BARRIER, NULL, VK_ACCESS_SHADER_WRITE_BIT,
            VK_ACCESS_SHADER_READ_BIT, VK_QUEUE_FAMILY_IGNORED, VK_QUEUE_FAMILY_IGNORED, c->data, 0, VK_WHOLE_SIZE };
        vkCmdPipelineBarrier(c->cb, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, 0,
                             0, NULL, 1, &bb, 0, NULL);
        break;
    }
    case B_MEM2: case B_VKD3D: case B_FULL: case B_VKD3D_PC: {
        VkMemoryBarrier2 mb = { VK_STRUCTURE_TYPE_MEMORY_BARRIER_2 };
        if (b == B_MEM2) {
            mb.srcStageMask = mb.dstStageMask = VK_PIPELINE_STAGE_2_COMPUTE_SHADER_BIT;
            mb.srcAccessMask = VK_ACCESS_2_SHADER_STORAGE_WRITE_BIT;
            mb.dstAccessMask = VK_ACCESS_2_SHADER_STORAGE_READ_BIT;
        } else if (b == B_FULL) {
            mb.srcStageMask = mb.dstStageMask = VK_PIPELINE_STAGE_2_ALL_COMMANDS_BIT;
            mb.srcAccessMask = VK_ACCESS_2_MEMORY_WRITE_BIT;
            mb.dstAccessMask = VK_ACCESS_2_MEMORY_READ_BIT | VK_ACCESS_2_MEMORY_WRITE_BIT;
        } else {
            mb.srcStageMask = mb.dstStageMask = vkd3d_stages;
            mb.srcAccessMask = mb.dstAccessMask = vkd3d_access;
        }
        VkDependencyInfo di = { VK_STRUCTURE_TYPE_DEPENDENCY_INFO, NULL, 0, 1, &mb, 0, NULL, 0, NULL };
        pCmdPipelineBarrier2(c->cb, &di);
        break;
    }
    default:
        break;
    }
}

/* Records n dispatches (n == 0: only the timestamps), submits reps + 1
 * times, returns the median GPU microseconds and the CPU recording time. */
static double run(const struct ctx *c, VkPipeline pipe, uint32_t grid, int indirect, enum bar b, int n,
                  double *rec_us)
{
    CHK(vkResetCommandBuffer(c->cb, 0));
    VkCommandBufferBeginInfo bi = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO };
    CHK(vkBeginCommandBuffer(c->cb, &bi));
    vkCmdResetQueryPool(c->cb, c->qp, 0, b == B_TS ? (uint32_t)n + 2 : 2);
    vkCmdBindPipeline(c->cb, VK_PIPELINE_BIND_POINT_COMPUTE, pipe);
    vkCmdBindDescriptorSets(c->cb, VK_PIPELINE_BIND_POINT_COMPUTE, c->pl, 0, 1, &c->ds, 0, NULL);
    uint32_t pc[2] = { (1u << 18) - 1, 0x9e3779b9u };
    vkCmdPushConstants(c->cb, c->pl, VK_SHADER_STAGE_COMPUTE_BIT, 0, sizeof(pc), pc);
    vkCmdWriteTimestamp(c->cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, c->qp, 0);
    double r0 = now_us();
    for (int k = 0; k < n; k++) {
        if (k > 0) emit_barrier(c, b);
        if (b == B_VKD3D_PC) {
            pc[1] = 0x9e3779b9u * (uint32_t)(k + 1);
            vkCmdPushConstants(c->cb, c->pl, VK_SHADER_STAGE_COMPUTE_BIT, 0, sizeof(pc), pc);
        }
        if (indirect)
            vkCmdDispatchIndirect(c->cb, c->args, 0);
        else
            vkCmdDispatch(c->cb, grid, 1, 1);
        if (b == B_TS)
            vkCmdWriteTimestamp(c->cb, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, c->qp, 2 + (uint32_t)k);
    }
    double r1 = now_us();
    vkCmdWriteTimestamp(c->cb, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, c->qp, 1);
    CHK(vkEndCommandBuffer(c->cb));
    if (rec_us) *rec_us = n ? (r1 - r0) / n : 0;

    double *v = malloc(sizeof(double) * (size_t)c->reps);
    for (int r = -1; r < c->reps; r++) {
        VkSubmitInfo si = { VK_STRUCTURE_TYPE_SUBMIT_INFO, NULL, 0, NULL, NULL, 1, &c->cb };
        CHK(vkQueueSubmit(c->q, 1, &si, VK_NULL_HANDLE));
        CHK(vkQueueWaitIdle(c->q));
        uint64_t ts[2];
        CHK(vkGetQueryPoolResults(c->dev, c->qp, 0, 2, sizeof(ts), ts, sizeof(uint64_t),
                                  VK_QUERY_RESULT_64_BIT | VK_QUERY_RESULT_WAIT_BIT));
        if (r >= 0) v[r] = (double)((ts[1] - ts[0]) & c->ts_mask) * c->ts_ns / 1000.0;
    }
    qsort(v, (size_t)c->reps, sizeof(double), cmp_d);
    double med = v[c->reps / 2];
    free(v);
    return med;
}

static void print_stats(VkDevice dev, VkPipeline pipe, const char *name)
{
    PFN_vkGetPipelineExecutablePropertiesKHR gp =
        (PFN_vkGetPipelineExecutablePropertiesKHR)vkGetDeviceProcAddr(dev, "vkGetPipelineExecutablePropertiesKHR");
    PFN_vkGetPipelineExecutableStatisticsKHR gs =
        (PFN_vkGetPipelineExecutableStatisticsKHR)vkGetDeviceProcAddr(dev, "vkGetPipelineExecutableStatisticsKHR");
    if (!gp || !gs) return;
    VkPipelineInfoKHR pi = { VK_STRUCTURE_TYPE_PIPELINE_INFO_KHR, NULL, pipe };
    uint32_t ne = 0;
    if (gp(dev, &pi, &ne, NULL) != VK_SUCCESS || ne == 0) return;
    VkPipelineExecutableInfoKHR ei = { VK_STRUCTURE_TYPE_PIPELINE_EXECUTABLE_INFO_KHR, NULL, pipe, 0 };
    uint32_t ns = 0;
    if (gs(dev, &ei, &ns, NULL) != VK_SUCCESS || ns == 0) return;
    VkPipelineExecutableStatisticKHR *st = calloc(ns, sizeof(*st));
    for (uint32_t i = 0; i < ns; i++) st[i].sType = VK_STRUCTURE_TYPE_PIPELINE_EXECUTABLE_STATISTIC_KHR;
    if (gs(dev, &ei, &ns, st) == VK_SUCCESS) {
        printf("  %-4s:", name);
        for (uint32_t i = 0; i < ns; i++) {
            switch (st[i].format) {
            case VK_PIPELINE_EXECUTABLE_STATISTIC_FORMAT_BOOL32_KHR: printf(" %s=%u;", st[i].name, st[i].value.b32); break;
            case VK_PIPELINE_EXECUTABLE_STATISTIC_FORMAT_INT64_KHR: printf(" %s=%lld;", st[i].name, (long long)st[i].value.i64); break;
            case VK_PIPELINE_EXECUTABLE_STATISTIC_FORMAT_UINT64_KHR: printf(" %s=%llu;", st[i].name, (unsigned long long)st[i].value.u64); break;
            case VK_PIPELINE_EXECUTABLE_STATISTIC_FORMAT_FLOAT64_KHR: printf(" %s=%.2f;", st[i].name, st[i].value.f64); break;
            default: break;
            }
        }
        printf("\n");
    }
    free(st);
}

int main(int argc, char **argv)
{
    const char *gpu = "", *filter = NULL; int n = 256, reps = 15, want_compute = 0;
    for (int a = 1; a < argc; a++) {
        if (!strcmp(argv[a], "-g") && a + 1 < argc) gpu = argv[++a];
        else if (!strcmp(argv[a], "-n") && a + 1 < argc) n = atoi(argv[++a]);
        else if (!strcmp(argv[a], "-r") && a + 1 < argc) reps = atoi(argv[++a]);
        else if (!strcmp(argv[a], "-q") && a + 1 < argc) want_compute = !strcmp(argv[++a], "compute");
        else if (!strcmp(argv[a], "-f") && a + 1 < argc) filter = argv[++a];
        else {
            fprintf(stderr, "usage: dispcost [-g GPU_NAME_SUBSTRING] [-n DISPATCHES] [-r SUBMITS] [-q gfx|compute] [-f ROW_FILTER]\n");
            return 2;
        }
    }
    if (n < 2) n = 2;
    if (reps < 1) reps = 1;
    if (!load_vk()) { fprintf(stderr, "no Vulkan loader\n"); return 2; }

    VkApplicationInfo ai = { VK_STRUCTURE_TYPE_APPLICATION_INFO, NULL, "dispcost", 1, NULL, 0, VK_API_VERSION_1_3 };
    VkInstanceCreateInfo ici = { VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, NULL, 0, &ai };
    direct_driver_chain(&ici);
    VkInstance inst; CHK(vkCreateInstance(&ici, NULL, &inst)); load_inst(inst);
    uint32_t np = 8; VkPhysicalDevice pds[8]; CHK(vkEnumeratePhysicalDevices(inst, &np, pds));
    VkPhysicalDevice pd = VK_NULL_HANDLE; VkPhysicalDeviceProperties pp;
    for (uint32_t i = 0; i < np && !pd; i++) {
        vkGetPhysicalDeviceProperties(pds[i], &pp);
        if (*gpu ? stristr(pp.deviceName, gpu) : (pp.vendorID == 0x10de || stristr(pp.deviceName, "NVK")))
            pd = pds[i];
    }
    if (!pd) {
        fprintf(stderr, "no device matches \"%s\"; devices:\n", gpu);
        for (uint32_t i = 0; i < np; i++) { vkGetPhysicalDeviceProperties(pds[i], &pp); fprintf(stderr, "  %s\n", pp.deviceName); }
        return 2;
    }
    VkPhysicalDeviceDriverProperties drv = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRIVER_PROPERTIES };
    VkPhysicalDeviceProperties2 pp2 = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2, &drv };
    vkGetPhysicalDeviceProperties2(pd, &pp2);
    pp = pp2.properties;
    if (pp.apiVersion < VK_API_VERSION_1_3) { fprintf(stderr, "%s: Vulkan 1.3 needed\n", pp.deviceName); return 2; }

    uint32_t next = 0; CHK(vkEnumerateDeviceExtensionProperties(pd, NULL, &next, NULL));
    VkExtensionProperties *exts = calloc(next, sizeof(*exts));
    CHK(vkEnumerateDeviceExtensionProperties(pd, NULL, &next, exts));
    int have_pep = 0;
    for (uint32_t i = 0; i < next; i++)
        if (!strcmp(exts[i].extensionName, VK_KHR_PIPELINE_EXECUTABLE_PROPERTIES_EXTENSION_NAME)) have_pep = 1;
    free(exts);

    VkPhysicalDevicePipelineExecutablePropertiesFeaturesKHR pepf = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PIPELINE_EXECUTABLE_PROPERTIES_FEATURES_KHR };
    VkPhysicalDeviceVulkan13Features f13 = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES, have_pep ? (void *)&pepf : NULL };
    VkPhysicalDeviceFeatures2 f2 = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2, &f13 };
    vkGetPhysicalDeviceFeatures2(pd, &f2);
    if (!f13.synchronization2) { fprintf(stderr, "no synchronization2\n"); return 2; }
    VkPhysicalDevicePipelineExecutablePropertiesFeaturesKHR pepf_en = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PIPELINE_EXECUTABLE_PROPERTIES_FEATURES_KHR, NULL, pepf.pipelineExecutableInfo };
    VkPhysicalDeviceVulkan13Features f13_en = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES, have_pep ? (void *)&pepf_en : NULL };
    f13_en.synchronization2 = VK_TRUE;
    VkPhysicalDeviceFeatures2 f2_en = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2, &f13_en };

    uint32_t nq = 16; VkQueueFamilyProperties qf[16]; vkGetPhysicalDeviceQueueFamilyProperties(pd, &nq, qf);
    uint32_t qfi = UINT32_MAX;
    for (uint32_t i = 0; i < nq && qfi == UINT32_MAX; i++) {
        int gfx = (qf[i].queueFlags & VK_QUEUE_GRAPHICS_BIT) != 0, cmp = (qf[i].queueFlags & VK_QUEUE_COMPUTE_BIT) != 0;
        if (cmp && qf[i].timestampValidBits && (want_compute ? !gfx : gfx)) qfi = i;
    }
    if (qfi == UINT32_MAX) { fprintf(stderr, "no %s queue family with timestamps\n", want_compute ? "compute-only" : "graphics"); return 2; }
    const char *ext_names[1] = { VK_KHR_PIPELINE_EXECUTABLE_PROPERTIES_EXTENSION_NAME };
    float prio = 1; VkDeviceQueueCreateInfo qci = { VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, NULL, 0, qfi, 1, &prio };
    VkDeviceCreateInfo dci = { VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, &f2_en, 0, 1, &qci, 0, NULL,
                               have_pep ? 1u : 0u, ext_names, NULL };
    struct ctx c = { 0 };
    CHK(vkCreateDevice(pd, &dci, NULL, &c.dev));
    vkGetDeviceQueue(c.dev, qfi, 0, &c.q);
    pCmdPipelineBarrier2 = (PFN_vkCmdPipelineBarrier2)vkGetDeviceProcAddr(c.dev, "vkCmdPipelineBarrier2");
    if (!pCmdPipelineBarrier2) { fprintf(stderr, "no vkCmdPipelineBarrier2\n"); return 2; }
    c.ts_ns = pp.limits.timestampPeriod;
    c.ts_mask = qf[qfi].timestampValidBits >= 64 ? ~0ull : (1ull << qf[qfi].timestampValidBits) - 1;
    c.reps = reps;

    printf("dispcost: %s, %s %s, queue family %u (%s), %d dispatches per command buffer, median of %d submits\n",
           pp.deviceName, drv.driverName, drv.driverInfo, qfi, want_compute ? "compute" : "graphics", n, reps);

    /* Buffers: data (one word per invocation, 1024 workgroups), src (1 MiB
     * of load data), args (the indirect grid) */
    VkPhysicalDeviceMemoryProperties mp; vkGetPhysicalDeviceMemoryProperties(pd, &mp);
    VkBuffer bufs[3]; VkDeviceSize sizes[3] = { 1024 * 64 * 4, 1u << 20, 16 };
    VkBufferUsageFlags usage[3] = {
        VK_BUFFER_USAGE_STORAGE_BUFFER_BIT | VK_BUFFER_USAGE_TRANSFER_DST_BIT,
        VK_BUFFER_USAGE_STORAGE_BUFFER_BIT | VK_BUFFER_USAGE_TRANSFER_DST_BIT,
        VK_BUFFER_USAGE_INDIRECT_BUFFER_BIT | VK_BUFFER_USAGE_TRANSFER_DST_BIT };
    for (int b = 0; b < 3; b++) {
        VkBufferCreateInfo bci = { VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, NULL, 0, sizes[b], usage[b], VK_SHARING_MODE_EXCLUSIVE };
        CHK(vkCreateBuffer(c.dev, &bci, NULL, &bufs[b]));
        VkMemoryRequirements mr; vkGetBufferMemoryRequirements(c.dev, bufs[b], &mr);
        VkMemoryAllocateInfo mai = { VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, NULL, mr.size,
            mem_type(&mp, mr.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT) };
        VkDeviceMemory mem; CHK(vkAllocateMemory(c.dev, &mai, NULL, &mem)); CHK(vkBindBufferMemory(c.dev, bufs[b], mem, 0));
    }
    c.data = bufs[0]; c.args = bufs[2];

    VkDescriptorSetLayoutBinding lb[2] = { { 0, VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 1, VK_SHADER_STAGE_COMPUTE_BIT, NULL },
                                           { 1, VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 1, VK_SHADER_STAGE_COMPUTE_BIT, NULL } };
    VkDescriptorSetLayoutCreateInfo lci = { VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO, NULL, 0, 2, lb };
    VkDescriptorSetLayout dsl; CHK(vkCreateDescriptorSetLayout(c.dev, &lci, NULL, &dsl));
    VkPushConstantRange pcr = { VK_SHADER_STAGE_COMPUTE_BIT, 0, 8 };
    VkPipelineLayoutCreateInfo plci = { VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO, NULL, 0, 1, &dsl, 1, &pcr };
    CHK(vkCreatePipelineLayout(c.dev, &plci, NULL, &c.pl));
    VkDescriptorPoolSize ps = { VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 2 };
    VkDescriptorPoolCreateInfo dpci = { VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO, NULL, 0, 1, 1, &ps };
    VkDescriptorPool dp; CHK(vkCreateDescriptorPool(c.dev, &dpci, NULL, &dp));
    VkDescriptorSetAllocateInfo dsai = { VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO, NULL, dp, 1, &dsl };
    CHK(vkAllocateDescriptorSets(c.dev, &dsai, &c.ds));
    VkDescriptorBufferInfo dbi[2] = { { bufs[0], 0, VK_WHOLE_SIZE }, { bufs[1], 0, VK_WHOLE_SIZE } };
    VkWriteDescriptorSet w[2];
    for (int b = 0; b < 2; b++)
        w[b] = (VkWriteDescriptorSet){ VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, NULL, c.ds, (uint32_t)b, 0, 1,
                                       VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, NULL, &dbi[b], NULL };
    vkUpdateDescriptorSets(c.dev, 2, w, 0, NULL);

    VkCommandPoolCreateInfo cpci = { VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, NULL, VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT, qfi };
    VkCommandPool cp; CHK(vkCreateCommandPool(c.dev, &cpci, NULL, &cp));
    VkCommandBufferAllocateInfo cbai = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, NULL, cp, VK_COMMAND_BUFFER_LEVEL_PRIMARY, 1 };
    CHK(vkAllocateCommandBuffers(c.dev, &cbai, &c.cb));
    VkQueryPoolCreateInfo qpci = { VK_STRUCTURE_TYPE_QUERY_POOL_CREATE_INFO, NULL, 0, VK_QUERY_TYPE_TIMESTAMP, (uint32_t)n + 2 };
    CHK(vkCreateQueryPool(c.dev, &qpci, NULL, &c.qp));

    /* Fill the buffers once */
    VkCommandBufferBeginInfo bi = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO, NULL, VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT };
    CHK(vkBeginCommandBuffer(c.cb, &bi));
    vkCmdFillBuffer(c.cb, bufs[0], 0, VK_WHOLE_SIZE, 1);
    vkCmdFillBuffer(c.cb, bufs[1], 0, VK_WHOLE_SIZE, 0x01234567u);
    CHK(vkEndCommandBuffer(c.cb));
    VkSubmitInfo si = { VK_STRUCTURE_TYPE_SUBMIT_INFO, NULL, 0, NULL, NULL, 1, &c.cb };
    CHK(vkQueueSubmit(c.q, 1, &si, VK_NULL_HANDLE)); CHK(vkQueueWaitIdle(c.q));

    struct { const char *name; const uint32_t *spv; size_t size; } sh[3] = {
        { "tiny", spv_tiny, sizeof(spv_tiny) }, { "mid", spv_mid, sizeof(spv_mid) }, { "big", spv_big, sizeof(spv_big) } };
    VkPipeline pipes[3];
    if (have_pep && pepf.pipelineExecutableInfo) printf("shader statistics (VK_KHR_pipeline_executable_properties):\n");
    for (int s = 0; s < 3; s++) {
        VkShaderModuleCreateInfo smci = { VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO, NULL, 0, sh[s].size, sh[s].spv };
        VkShaderModule sm; CHK(vkCreateShaderModule(c.dev, &smci, NULL, &sm));
        VkComputePipelineCreateInfo cpi = { VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO, NULL,
            (have_pep && pepf.pipelineExecutableInfo) ? VK_PIPELINE_CREATE_CAPTURE_STATISTICS_BIT_KHR : 0,
            { VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, NULL, 0, VK_SHADER_STAGE_COMPUTE_BIT, sm, "main", NULL },
            c.pl, VK_NULL_HANDLE, 0 };
        CHK(vkCreateComputePipelines(c.dev, VK_NULL_HANDLE, 1, &cpi, NULL, &pipes[s]));
        if (have_pep && pepf.pipelineExecutableInfo) print_stats(c.dev, pipes[s], sh[s].name);
    }

    double empty = run(&c, pipes[0], 1, 0, B_NONE, 0, NULL);
    printf("empty command buffer (2 timestamps): %.2f us; columns: us per dispatch = (T - empty) / %d\n", empty, n);
    printf("%-5s %5s %-8s", "shader", "grid", "mode");
    for (int b = 0; b < B_COUNT; b++) printf(" %8s", bar_name[b]);
    printf(" %8s\n", "cpu");
    fflush(stdout);

    static const uint32_t grids[4] = { 1, 5, 64, 1024 };
    for (int s = 0; s < 3; s++)
        for (int g = 0; g < 4; g++)
            for (int ind = 0; ind < 2; ind++) {
                char row[64];
                snprintf(row, sizeof(row), "%s %u %s", sh[s].name, grids[g], ind ? "indirect" : "direct");
                if (filter && !strstr(row, filter)) continue;
                /* The indirect grid in the args buffer, before the timed stream */
                CHK(vkResetCommandBuffer(c.cb, 0));
                CHK(vkBeginCommandBuffer(c.cb, &bi));
                uint32_t gx[3] = { grids[g], 1, 1 };
                for (int k = 0; k < 3; k++) vkCmdFillBuffer(c.cb, c.args, 4 * k, 4, gx[k]);
                CHK(vkEndCommandBuffer(c.cb));
                CHK(vkQueueSubmit(c.q, 1, &si, VK_NULL_HANDLE)); CHK(vkQueueWaitIdle(c.q));

                printf("%-6s %5u %-8s", sh[s].name, grids[g], ind ? "indirect" : "direct");
                double cpu = 0;
                for (int b = 0; b < B_COUNT; b++) {
                    double t = run(&c, pipes[s], grids[g], ind, (enum bar)b, n, b == B_VKD3D_PC ? &cpu : NULL);
                    printf(" %8.2f", (t - empty) / n);
                    fflush(stdout);
                }
                printf(" %8.2f\n", cpu);
                fflush(stdout);
            }
    vkDestroyDevice(c.dev, NULL);
    vkDestroyInstance(inst, NULL);
    return 0;
}
