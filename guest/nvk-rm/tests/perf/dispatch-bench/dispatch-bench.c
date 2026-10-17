// SPDX-License-Identifier: MIT
// dispatch-bench: per-dispatch overhead of tiny compute dispatches.
//
// One queue, one command buffer per measurement holding N dispatches between two timestamps. Patterns:
//   none     bind once, N back-to-back dispatches
//   barrier  compute->compute memory barrier (SHADER_WRITE -> SHADER_READ|WRITE) between dispatches
//   push     vkCmdPushConstants between dispatches (vkd3d root constants)
//   bind     vkCmdBindDescriptorSets (alternating two sets) + push constants between dispatches
//   pipe     alternate between two pipelines (same shader, different spec constant) every dispatch
//   bpush    barrier + push constants between dispatches
//   vbar     vkd3d-proton UAV barrier: stages = all queue shader stages + TRANSFER, access SHADER_READ|WRITE|TRANSFER_WRITE,
//            + push constants (what a D3D12 UAV barrier + new root constants becomes)
//   ts       a BOTTOM_OF_PIPE timestamp after every dispatch (what a per-dispatch GPU profiler does)
// Shaders: "simple" (one SSBO) and "rich" (push constants + SSBO + SSBO + UBO).
// Reported per dispatch: GPU time (timestamps), CPU submit-to-fence time, CPU record time. Median of reps.
//
//   dispatch-bench [-n N] [-r reps] [-q u|c] [--dev STR] [--grids 1,4,64,1024] [--csv file]
#define _GNU_SOURCE
#ifdef _WIN32
#include <windows.h>
#else
#include <dlfcn.h>
#include <time.h>
#endif
#define VK_NO_PROTOTYPES
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include "cs_simple.h"
#include "cs_rich.h"

#define VKFUNCS(X) \
 X(vkDestroyInstance) X(vkEnumeratePhysicalDevices) X(vkGetPhysicalDeviceProperties) \
 X(vkGetPhysicalDeviceMemoryProperties) X(vkGetPhysicalDeviceQueueFamilyProperties) X(vkCreateDevice) \
 X(vkDestroyDevice) X(vkGetDeviceQueue) X(vkCreateDescriptorSetLayout) X(vkCreatePipelineLayout) X(vkCreateShaderModule) \
 X(vkCreateComputePipelines) X(vkCreateDescriptorPool) X(vkAllocateDescriptorSets) X(vkUpdateDescriptorSets) \
 X(vkCreateBuffer) X(vkGetBufferMemoryRequirements) X(vkAllocateMemory) X(vkBindBufferMemory) \
 X(vkCreateCommandPool) X(vkAllocateCommandBuffers) X(vkResetCommandPool) \
 X(vkBeginCommandBuffer) X(vkEndCommandBuffer) X(vkCmdBindPipeline) X(vkCmdBindDescriptorSets) \
 X(vkCmdPushConstants) X(vkCmdDispatch) X(vkCmdPipelineBarrier) X(vkCmdResetQueryPool) X(vkCmdWriteTimestamp) \
 X(vkCreateQueryPool) X(vkGetQueryPoolResults) X(vkQueueSubmit) X(vkCreateFence) X(vkWaitForFences) X(vkResetFences) \
 X(vkDeviceWaitIdle)
#define X(n) static PFN_##n n;
VKFUNCS(X)
#undef X
static PFN_vkGetInstanceProcAddr gipa;

#define VK(x) do { VkResult r_ = (x); if (r_ != VK_SUCCESS) { fprintf(stderr, "%s:%d %s -> %d\n", __FILE__, __LINE__, #x, (int)r_); exit(1); } } while (0)

static double now_us(void) {
#ifdef _WIN32
    static LARGE_INTEGER f; LARGE_INTEGER c; if (!f.QuadPart) QueryPerformanceFrequency(&f); QueryPerformanceCounter(&c);
    return 1e6 * (double)c.QuadPart / (double)f.QuadPart;
#else
    struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec * 1e6 + t.tv_nsec / 1e3;
#endif
}

static int cmpd(const void *a, const void *b) { double x = *(const double *)a, y = *(const double *)b; return x < y ? -1 : x > y; }
static double median(double *v, int n) { qsort(v, n, sizeof *v, cmpd); return n & 1 ? v[n / 2] : 0.5 * (v[n / 2 - 1] + v[n / 2]); }

enum { P_NONE, P_BARRIER, P_PUSH, P_BIND, P_PIPE, P_BPUSH, P_VBAR, P_TS, P_COUNT };
static const char *PNAME[] = { "none", "barrier", "push", "bind", "pipe", "bpush", "vbar", "ts" };

static VkDevice dev;
static VkPhysicalDeviceMemoryProperties mp;

static VkBuffer mkbuf(VkDeviceSize size, VkBufferUsageFlags usage) {
    VkBufferCreateInfo bi = { VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, NULL, 0, size, usage, VK_SHARING_MODE_EXCLUSIVE };
    VkBuffer b; VK(vkCreateBuffer(dev, &bi, NULL, &b));
    VkMemoryRequirements mr; vkGetBufferMemoryRequirements(dev, b, &mr);
    uint32_t ti = UINT32_MAX;
    for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
        if ((mr.memoryTypeBits & (1u << i)) && (mp.memoryTypes[i].propertyFlags & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT)) { ti = i; break; }
    if (ti == UINT32_MAX) for (uint32_t i = 0; i < mp.memoryTypeCount; i++) if (mr.memoryTypeBits & (1u << i)) { ti = i; break; }
    VkMemoryAllocateInfo ai = { VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, NULL, mr.size, ti };
    VkDeviceMemory m; VK(vkAllocateMemory(dev, &ai, NULL, &m)); VK(vkBindBufferMemory(dev, b, m, 0));
    return b;
}

int main(int argc, char **argv) {
    int N = 1000, reps = 15, warm = 3; char qsel = 'u'; const char *devstr = NULL, *csv = NULL;
    uint32_t grids[8] = { 1, 4, 64, 1024 }; int ngrids = 4;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "-n") && i + 1 < argc) N = atoi(argv[++i]);
        else if (!strcmp(argv[i], "-r") && i + 1 < argc) reps = atoi(argv[++i]);
        else if (!strcmp(argv[i], "-q") && i + 1 < argc) qsel = argv[++i][0];
        else if (!strcmp(argv[i], "--dev") && i + 1 < argc) devstr = argv[++i];
        else if (!strcmp(argv[i], "--csv") && i + 1 < argc) csv = argv[++i];
        else if (!strcmp(argv[i], "--grids") && i + 1 < argc) {
            ngrids = 0; for (char *s = argv[++i]; *s && ngrids < 8;) { grids[ngrids++] = (uint32_t)strtoul(s, &s, 10); if (*s == ',') s++; else break; }
        } else {
            fprintf(stderr, "usage: %s [-n N(1000)] [-r reps(15)] [-q u|c (universal or compute-only queue family)] [--dev STR] "
                            "[--grids 1,4,64,1024] [--csv file]\n", argv[0]);
            return 2;
        }
    }
    if (N < 1 || reps < 1 || reps > 1000) return 2;

#ifdef _WIN32
    HMODULE lib = LoadLibraryA("vulkan-1.dll"); if (lib) gipa = (PFN_vkGetInstanceProcAddr)(void *)GetProcAddress(lib, "vkGetInstanceProcAddr");
#else
    void *lib = dlopen("libvulkan.so.1", RTLD_NOW); if (lib) gipa = (PFN_vkGetInstanceProcAddr)dlsym(lib, "vkGetInstanceProcAddr");
#endif
    if (!gipa) { fprintf(stderr, "cannot load the Vulkan loader\n"); return 1; }
    PFN_vkCreateInstance pci = (PFN_vkCreateInstance)gipa(NULL, "vkCreateInstance");
    VkApplicationInfo appi = { VK_STRUCTURE_TYPE_APPLICATION_INFO, NULL, "dispatch-bench", 1, NULL, 0, VK_API_VERSION_1_1 };
    VkInstanceCreateInfo ici = { VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, NULL, 0, &appi };
    VkInstance inst; VK(pci(&ici, NULL, &inst));
#define X(n) n = (PFN_##n)gipa(inst, #n);
    VKFUNCS(X)
#undef X

    uint32_t np = 8; VkPhysicalDevice pds[8]; VK(vkEnumeratePhysicalDevices(inst, &np, pds));
    VkPhysicalDevice pd = VK_NULL_HANDLE; VkPhysicalDeviceProperties pp;
    for (uint32_t i = 0; i < np; i++) {
        vkGetPhysicalDeviceProperties(pds[i], &pp);
        int ok = devstr ? strstr(pp.deviceName, devstr) != NULL
                        : (pp.vendorID == 0x10de || strstr(pp.deviceName, "NVK") || strstr(pp.deviceName, "Helios"));
        if (ok) { pd = pds[i]; break; }
    }
    if (!pd) { fprintf(stderr, "no matching device (use --dev)\n"); return 1; }
    vkGetPhysicalDeviceMemoryProperties(pd, &mp);
    uint32_t nq = 16; VkQueueFamilyProperties qfp[16]; vkGetPhysicalDeviceQueueFamilyProperties(pd, &nq, qfp);
    uint32_t qf = UINT32_MAX;
    for (uint32_t i = 0; i < nq; i++) {
        VkQueueFlags f = qfp[i].queueFlags;
        if (!(f & VK_QUEUE_COMPUTE_BIT)) continue;
        if (qsel == 'c' && !(f & VK_QUEUE_GRAPHICS_BIT)) { qf = i; break; }
        if (qsel != 'c' && (f & VK_QUEUE_GRAPHICS_BIT)) { qf = i; break; }
    }
    if (qf == UINT32_MAX) { fprintf(stderr, "no %s queue family\n", qsel == 'c' ? "compute-only" : "graphics+compute"); return 1; }
    if (!qfp[qf].timestampValidBits) { fprintf(stderr, "queue family %u has no timestamps\n", qf); return 1; }
    uint64_t tsmask = qfp[qf].timestampValidBits >= 64 ? ~0ull : ((1ull << qfp[qf].timestampValidBits) - 1);
    double period_us = pp.limits.timestampPeriod / 1000.0;
    printf("# device: %s  vendor 0x%04x  driver 0x%08x  api %u.%u.%u  queue family %u (%s)  ts period %.3f ns\n",
           pp.deviceName, pp.vendorID, pp.driverVersion, VK_VERSION_MAJOR(pp.apiVersion), VK_VERSION_MINOR(pp.apiVersion),
           VK_VERSION_PATCH(pp.apiVersion), qf, (qfp[qf].queueFlags & VK_QUEUE_GRAPHICS_BIT) ? "graphics+compute" : "compute-only",
           pp.limits.timestampPeriod);
    printf("# N=%d dispatches per command buffer, %d reps (median), %d warmup, local size 64x1x1\n", N, reps, warm);

    float prio = 1.0f;
    VkDeviceQueueCreateInfo qci = { VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, NULL, 0, qf, 1, &prio };
    VkDeviceCreateInfo dci = { VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, NULL, 0, 1, &qci };
    VK(vkCreateDevice(pd, &dci, NULL, &dev));
    VkQueue q; vkGetDeviceQueue(dev, qf, 0, &q);

    // Resources: out SSBO, in SSBO, UBO; two descriptor sets pointing at different offsets of out.
    VkBuffer bout = mkbuf(64 * 1024, VK_BUFFER_USAGE_STORAGE_BUFFER_BIT);
    VkBuffer bin = mkbuf(4096, VK_BUFFER_USAGE_STORAGE_BUFFER_BIT);
    VkBuffer bub = mkbuf(256, VK_BUFFER_USAGE_UNIFORM_BUFFER_BIT);
    VkDescriptorSetLayoutBinding lb[3] = {
        { 0, VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 1, VK_SHADER_STAGE_COMPUTE_BIT, NULL },
        { 1, VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 1, VK_SHADER_STAGE_COMPUTE_BIT, NULL },
        { 2, VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER, 1, VK_SHADER_STAGE_COMPUTE_BIT, NULL } };
    VkDescriptorSetLayoutCreateInfo dli = { VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO, NULL, 0, 3, lb };
    VkDescriptorSetLayout dsl; VK(vkCreateDescriptorSetLayout(dev, &dli, NULL, &dsl));
    VkPushConstantRange pcr = { VK_SHADER_STAGE_COMPUTE_BIT, 0, 32 };
    VkPipelineLayoutCreateInfo pli = { VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO, NULL, 0, 1, &dsl, 1, &pcr };
    VkPipelineLayout pl; VK(vkCreatePipelineLayout(dev, &pli, NULL, &pl));
    VkDescriptorPoolSize ps[2] = { { VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 4 }, { VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER, 2 } };
    VkDescriptorPoolCreateInfo dpi = { VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO, NULL, 0, 2, 2, ps };
    VkDescriptorPool dp; VK(vkCreateDescriptorPool(dev, &dpi, NULL, &dp));
    VkDescriptorSetLayout dsls[2] = { dsl, dsl };
    VkDescriptorSetAllocateInfo dai = { VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO, NULL, dp, 2, dsls };
    VkDescriptorSet ds[2]; VK(vkAllocateDescriptorSets(dev, &dai, ds));
    for (int s = 0; s < 2; s++) {
        VkDescriptorBufferInfo bi[3] = { { bout, (VkDeviceSize)s * 32768, 32768 }, { bin, 0, 4096 }, { bub, 0, 256 } };
        VkWriteDescriptorSet w[3];
        for (int k = 0; k < 3; k++) {
            VkWriteDescriptorSet t = { VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, NULL, ds[s], (uint32_t)k, 0, 1,
                                       k == 2 ? VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER : VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, NULL, &bi[k], NULL };
            w[k] = t;
        }
        vkUpdateDescriptorSets(dev, 3, w, 0, NULL);
    }

    // Pipelines: [shader][variant] with spec constant K = 1 / 2.
    VkPipeline pipes[2][2];
    const uint32_t *code[2] = { (const uint32_t *)cs_simple_spv, (const uint32_t *)cs_rich_spv };
    size_t codesz[2] = { cs_simple_spv_len, cs_rich_spv_len };
    for (int sh = 0; sh < 2; sh++) {
        uint32_t *aligned = malloc(codesz[sh]); memcpy(aligned, code[sh], codesz[sh]);
        VkShaderModuleCreateInfo smi = { VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO, NULL, 0, codesz[sh], aligned };
        VkShaderModule sm; VK(vkCreateShaderModule(dev, &smi, NULL, &sm)); free(aligned);
        for (int v = 0; v < 2; v++) {
            uint32_t kval = 1 + v;
            VkSpecializationMapEntry me = { 0, 0, 4 };
            VkSpecializationInfo si = { 1, &me, 4, &kval };
            VkComputePipelineCreateInfo cpi = { VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO, NULL, 0,
                { VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, NULL, 0, VK_SHADER_STAGE_COMPUTE_BIT, sm, "main", &si }, pl, VK_NULL_HANDLE, 0 };
            VK(vkCreateComputePipelines(dev, VK_NULL_HANDLE, 1, &cpi, NULL, &pipes[sh][v]));
        }
    }

    VkQueryPoolCreateInfo qpi = { VK_STRUCTURE_TYPE_QUERY_POOL_CREATE_INFO, NULL, 0, VK_QUERY_TYPE_TIMESTAMP, (uint32_t)N + 2 };
    VkQueryPool qp; VK(vkCreateQueryPool(dev, &qpi, NULL, &qp));
    VkCommandPoolCreateInfo cpci = { VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, NULL, VK_COMMAND_POOL_CREATE_TRANSIENT_BIT, qf };
    VkCommandPool cp; VK(vkCreateCommandPool(dev, &cpci, NULL, &cp));
    VkCommandBufferAllocateInfo cbai = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, NULL, cp, VK_COMMAND_BUFFER_LEVEL_PRIMARY, 1 };
    VkCommandBuffer cb; VK(vkAllocateCommandBuffers(dev, &cbai, &cb));
    VkFenceCreateInfo fci = { VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
    VkFence fence; VK(vkCreateFence(dev, &fci, NULL, &fence));

    FILE *cf = csv ? fopen(csv, "w") : NULL;
    if (cf) fprintf(cf, "shader\tpattern\tgrid\tN\tgpu_us_per_disp\tgpu_min_us_per_disp\tsubmit_fence_us_per_disp\trecord_us_per_disp\n");

    struct { int sh, pat; } cfgs[] = {
        { 0, P_NONE }, { 0, P_BARRIER }, { 0, P_PIPE }, { 0, P_TS },
        { 1, P_NONE }, { 1, P_BARRIER }, { 1, P_PUSH }, { 1, P_BIND }, { 1, P_PIPE }, { 1, P_BPUSH }, { 1, P_VBAR } };
    int ncfg = (int)(sizeof cfgs / sizeof cfgs[0]);
    static const char *SHN[] = { "simple", "rich" };
    VkMemoryBarrier mb = { VK_STRUCTURE_TYPE_MEMORY_BARRIER, NULL, VK_ACCESS_SHADER_WRITE_BIT, VK_ACCESS_SHADER_READ_BIT | VK_ACCESS_SHADER_WRITE_BIT };
    uint32_t pcdata[8] = { 0, 1, 2, 3, 4, 5, 6, 7 };
    int universal = (qfp[qf].queueFlags & VK_QUEUE_GRAPHICS_BIT) != 0;
    VkPipelineStageFlags vstage = VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT | VK_PIPELINE_STAGE_TRANSFER_BIT |
        (universal ? VK_PIPELINE_STAGE_VERTEX_SHADER_BIT | VK_PIPELINE_STAGE_TESSELLATION_CONTROL_SHADER_BIT |
                     VK_PIPELINE_STAGE_TESSELLATION_EVALUATION_SHADER_BIT | VK_PIPELINE_STAGE_GEOMETRY_SHADER_BIT |
                     VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT : 0);
    VkAccessFlags vacc = VK_ACCESS_SHADER_READ_BIT | VK_ACCESS_SHADER_WRITE_BIT | VK_ACCESS_TRANSFER_WRITE_BIT;
    VkMemoryBarrier vmb = { VK_STRUCTURE_TYPE_MEMORY_BARRIER, NULL, vacc, vacc };

    // Empty command buffer baseline (two timestamps, no dispatches): fixed cost to put the per-dispatch numbers in context.
    printf("%-7s %-8s %5s | %9s %9s | %11s %9s\n", "shader", "pattern", "grid", "gpu_us/d", "gpu_min", "subm2fence", "record");
    double *g = malloc(sizeof(double) * reps), *c = malloc(sizeof(double) * reps), *rcd = malloc(sizeof(double) * reps);
    for (int ci = -1; ci < ncfg; ci++) {
        for (int gi = 0; gi < ngrids; gi++) {
            if (ci < 0 && gi > 0) break;
            int sh = ci < 0 ? 0 : cfgs[ci].sh, pat = ci < 0 ? P_NONE : cfgs[ci].pat, n = ci < 0 ? 0 : N;
            uint32_t grid = grids[gi];
            for (int r = -warm; r < reps; r++) {
                VK(vkResetCommandPool(dev, cp, 0));
                double t0 = now_us();
                VkCommandBufferBeginInfo cbi = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO, NULL, VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT };
                VK(vkBeginCommandBuffer(cb, &cbi));
                vkCmdResetQueryPool(cb, qp, 0, pat == P_TS ? (uint32_t)N + 2 : 2);
                vkCmdBindPipeline(cb, VK_PIPELINE_BIND_POINT_COMPUTE, pipes[sh][0]);
                vkCmdBindDescriptorSets(cb, VK_PIPELINE_BIND_POINT_COMPUTE, pl, 0, 1, &ds[0], 0, NULL);
                vkCmdPushConstants(cb, pl, VK_SHADER_STAGE_COMPUTE_BIT, 0, 32, pcdata);
                vkCmdWriteTimestamp(cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, qp, 0);
                for (int d = 0; d < n; d++) {
                    if (d > 0) {
                        if (pat == P_BARRIER || pat == P_BPUSH)
                            vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, 0, 1, &mb, 0, NULL, 0, NULL);
                        if (pat == P_VBAR) vkCmdPipelineBarrier(cb, vstage, vstage, 0, 1, &vmb, 0, NULL, 0, NULL);
                        if (pat == P_BIND) vkCmdBindDescriptorSets(cb, VK_PIPELINE_BIND_POINT_COMPUTE, pl, 0, 1, &ds[d & 1], 0, NULL);
                        if (pat == P_PUSH || pat == P_BIND || pat == P_BPUSH || pat == P_VBAR) {
                            pcdata[0] = (uint32_t)d; pcdata[7] = (uint32_t)d * 3u;
                            vkCmdPushConstants(cb, pl, VK_SHADER_STAGE_COMPUTE_BIT, 0, 32, pcdata);
                        }
                        if (pat == P_PIPE) vkCmdBindPipeline(cb, VK_PIPELINE_BIND_POINT_COMPUTE, pipes[sh][d & 1]);
                    }
                    vkCmdDispatch(cb, grid, 1, 1);
                    if (pat == P_TS) vkCmdWriteTimestamp(cb, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, qp, 2 + (uint32_t)d);
                }
                vkCmdWriteTimestamp(cb, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, qp, 1);
                VK(vkEndCommandBuffer(cb));
                double t1 = now_us();
                VkSubmitInfo si = { VK_STRUCTURE_TYPE_SUBMIT_INFO, NULL, 0, NULL, NULL, 1, &cb };
                VK(vkResetFences(dev, 1, &fence));
                double t2 = now_us();
                VK(vkQueueSubmit(q, 1, &si, fence));
                VK(vkWaitForFences(dev, 1, &fence, VK_TRUE, 10ull * 1000 * 1000 * 1000));
                double t3 = now_us();
                uint64_t ts[2]; VK(vkGetQueryPoolResults(dev, qp, 0, 2, sizeof ts, ts, 8, VK_QUERY_RESULT_64_BIT | VK_QUERY_RESULT_WAIT_BIT));
                if (r < 0) continue;
                double div = n ? n : 1;
                g[r] = (double)((ts[1] - ts[0]) & tsmask) * period_us / div;
                c[r] = (t3 - t2) / div;
                rcd[r] = (t1 - t0) / div;
            }
            double gmed = median(g, reps), gmin = g[0], cmed = median(c, reps), rmed = median(rcd, reps);
            const char *shn = ci < 0 ? "-" : SHN[sh], *pn = ci < 0 ? "empty" : PNAME[pat];
            if (ci < 0)
                printf("%-7s %-8s %5s | %9.3f %9.3f | %11.3f %9.3f   (whole command buffer, us)\n", shn, pn, "-", gmed, gmin, cmed, rmed);
            else
                printf("%-7s %-8s %5u | %9.3f %9.3f | %11.3f %9.3f\n", shn, pn, grid, gmed, gmin, cmed, rmed);
            fflush(stdout);
            if (cf) fprintf(cf, "%s\t%s\t%u\t%d\t%.4f\t%.4f\t%.4f\t%.4f\n", shn, pn, ci < 0 ? 0 : grid, n, gmed, gmin, cmed, rmed);
        }
    }
    if (cf) fclose(cf);
    VK(vkDeviceWaitIdle(dev));
    printf("# gpu_us/d = GPU timestamp delta / N (median), gpu_min = fastest rep; subm2fence = CPU vkQueueSubmit..fence signalled / N; "
           "record = CPU command buffer record time / N\n");
    return 0;
}
