// nvstats: compile vkd3d-proton SPIR-V compute shaders with the host Vulkan driver (NVIDIA) and print the
// driver's own pipeline statistics (VK_KHR_pipeline_executable_properties): registers, instructions, spills.
// Black-box: we only read what the driver reports for our shaders.
//   nvstats file.spv [file.spv ...]
#ifdef _WIN32
#define VK_NO_PROTOTYPES
#include <windows.h>
#endif
#include <vulkan/vulkan.h>
#ifdef _WIN32
#define F(n) static PFN_##n n;
F(vkCreateInstance) F(vkEnumeratePhysicalDevices) F(vkGetPhysicalDeviceProperties) F(vkGetPhysicalDeviceFeatures2)
F(vkCreateDevice) F(vkGetDeviceProcAddr) F(vkCreateDescriptorSetLayout) F(vkCreatePipelineLayout) F(vkCreateShaderModule)
F(vkCreateComputePipelines) F(vkCreateGraphicsPipelines) F(vkDestroyPipeline) F(vkDestroyShaderModule) F(vkDestroyPipelineLayout)
F(vkDestroyDescriptorSetLayout) F(vkDestroyDevice) F(vkDestroyInstance)
static PFN_vkGetInstanceProcAddr gipa;
static void load_global(void) { HMODULE m = LoadLibraryA("vulkan-1.dll"); gipa = (PFN_vkGetInstanceProcAddr)GetProcAddress(m, "vkGetInstanceProcAddr");
  vkCreateInstance = (PFN_vkCreateInstance)gipa(NULL, "vkCreateInstance"); }
#define L(n) n = (PFN_##n)gipa(inst, #n);
static void load_inst(VkInstance inst) { L(vkEnumeratePhysicalDevices) L(vkGetPhysicalDeviceProperties) L(vkGetPhysicalDeviceFeatures2)
  L(vkCreateDevice) L(vkGetDeviceProcAddr) L(vkCreateDescriptorSetLayout) L(vkCreatePipelineLayout) L(vkCreateShaderModule)
  L(vkCreateComputePipelines) L(vkCreateGraphicsPipelines) L(vkDestroyPipeline) L(vkDestroyShaderModule) L(vkDestroyPipelineLayout)
  L(vkDestroyDescriptorSetLayout) L(vkDestroyDevice) L(vkDestroyInstance) }
#else
static void load_global(void) {}
static void load_inst(VkInstance i) { (void)i; }
#endif
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>

#define CHK(x) do { VkResult r_ = (x); if (r_ != VK_SUCCESS) { fprintf(stderr, "%s -> %d\n", #x, r_); return r_; } } while (0)

typedef struct { uint32_t op, w[8]; } Ins;
static uint32_t *spv; static size_t nw;

/* --- tiny SPIR-V reflection: descriptor set/binding -> descriptor types --------------------------------------- */
#define MAXID (1u << 20)
static uint32_t *dset, *dbind, *ty_op, *ty_a, *ty_b, *ty_c, *var_sc, *var_ty, *blockdec, *bufblock, *constval;
static uint8_t *hasset;

static VkDescriptorType image_type(uint32_t t) {
    // OpTypeImage: result, sampled type, Dim, Depth, Arrayed, MS, Sampled, Format
    uint32_t dim = ty_a[t], sampled = ty_b[t];
    if (dim == 5 /* Buffer */) return sampled == 2 ? VK_DESCRIPTOR_TYPE_STORAGE_TEXEL_BUFFER : VK_DESCRIPTOR_TYPE_UNIFORM_TEXEL_BUFFER;
    return sampled == 2 ? VK_DESCRIPTOR_TYPE_STORAGE_IMAGE : VK_DESCRIPTOR_TYPE_SAMPLED_IMAGE;
}

struct bind { uint32_t set, binding, count; int ntypes; VkDescriptorType types[8]; };

static int reflect(struct bind *out, int maxb, uint32_t *model, uint32_t ls[3]) {
    memset(hasset, 0, MAXID);
    int n = 0; ls[0] = ls[1] = ls[2] = 1;
    for (size_t i = 5; i < nw;) {
        uint32_t wc = spv[i] >> 16, op = spv[i] & 0xffff; uint32_t *a = &spv[i + 1];
        switch (op) {
        case 15: *model = a[0]; break;                                  // OpEntryPoint
        case 16: if (a[1] == 17) { ls[0] = a[2]; ls[1] = a[3]; ls[2] = a[4]; } break; // ExecutionMode LocalSize
        case 71:                                                        // OpDecorate
            if (a[1] == 34) { dset[a[0]] = a[2]; hasset[a[0]] = 1; }
            else if (a[1] == 33) dbind[a[0]] = a[2];
            else if (a[1] == 2) blockdec[a[0]] = 1;
            else if (a[1] == 3) bufblock[a[0]] = 1;
            break;
        case 25: ty_op[a[0]] = 25; ty_a[a[0]] = a[2]; ty_b[a[0]] = a[6]; break;   // OpTypeImage
        case 26: ty_op[a[0]] = 26; break;                                         // OpTypeSampler
        case 27: ty_op[a[0]] = 27; ty_a[a[0]] = a[1]; break;                      // OpTypeSampledImage
        case 28: ty_op[a[0]] = 28; ty_a[a[0]] = a[1]; ty_c[a[0]] = a[2]; break;   // OpTypeArray (len id)
        case 29: ty_op[a[0]] = 29; ty_a[a[0]] = a[1]; break;                      // OpTypeRuntimeArray
        case 30: ty_op[a[0]] = 30; break;                                         // OpTypeStruct
        case 32: ty_op[a[0]] = 32; ty_a[a[0]] = a[1]; ty_b[a[0]] = a[2]; break;   // OpTypePointer sc, type
        case 5341: ty_op[a[0]] = 5341; break;                                     // OpTypeAccelerationStructureKHR
        case 43: constval[a[1]] = a[2]; break;                                    // OpConstant
        case 59: var_ty[a[1]] = a[0]; var_sc[a[1]] = a[2]; break;                 // OpVariable type, id, sc
        }
        i += wc ? wc : 1;
    }
    for (uint32_t id = 1; id < MAXID && id < spv[3]; id++) {
        if (!hasset[id] || !var_ty[id]) continue;
        uint32_t p = var_ty[id], sc = var_sc[id], t = ty_b[p], count = 1;
        if (ty_op[t] == 29) { count = 16384; t = ty_a[t]; }
        else if (ty_op[t] == 28) { count = constval[ty_c[t]]; t = ty_a[t]; }
        VkDescriptorType dt;
        if (ty_op[t] == 25) dt = image_type(t);
        else if (ty_op[t] == 26) dt = VK_DESCRIPTOR_TYPE_SAMPLER;
        else if (ty_op[t] == 27) dt = VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER;
        else if (ty_op[t] == 5341) dt = VK_DESCRIPTOR_TYPE_ACCELERATION_STRUCTURE_KHR;
        else if (sc == 12 /* StorageBuffer */ || (sc == 2 && bufblock[t])) dt = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER;
        else if (sc == 2 /* Uniform */) dt = VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER;
        else continue;
        int k; for (k = 0; k < n; k++) if (out[k].set == dset[id] && out[k].binding == dbind[id]) break;
        if (k == n) { if (n == maxb) continue; out[n] = (struct bind){ dset[id], dbind[id], count, 0, {0} }; n++; }
        if (count > out[k].count) out[k].count = count;
        int seen = 0; for (int j = 0; j < out[k].ntypes; j++) seen |= out[k].types[j] == dt;
        if (!seen && out[k].ntypes < 8) out[k].types[out[k].ntypes++] = dt;
    }
    return n;
}

static void *rd(const char *p, size_t *n) {
    FILE *f = fopen(p, "rb"); if (!f) return NULL; fseek(f, 0, SEEK_END); *n = ftell(f); fseek(f, 0, SEEK_SET);
    void *b = malloc(*n); if (fread(b, 1, *n, f) != *n) { free(b); b = NULL; } fclose(f); return b;
}


static VkShaderModule mkmod(VkDevice dev, const char *p) {
    size_t n; void *b = rd(p, &n); if (!b) return VK_NULL_HANDLE;
    VkShaderModuleCreateInfo ci = { VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO, NULL, 0, n, b };
    VkShaderModule m = VK_NULL_HANDLE; vkCreateShaderModule(dev, &ci, NULL, &m); free(b); return m;
}
int main(int argc, char **argv) {
    dset = calloc(MAXID, 4); dbind = calloc(MAXID, 4); ty_op = calloc(MAXID, 4); ty_a = calloc(MAXID, 4);
    ty_b = calloc(MAXID, 4); ty_c = calloc(MAXID, 4); var_sc = calloc(MAXID, 4); var_ty = calloc(MAXID, 4);
    blockdec = calloc(MAXID, 4); bufblock = calloc(MAXID, 4); constval = calloc(MAXID, 4); hasset = calloc(MAXID, 1);

    load_global();
    VkApplicationInfo ai = { VK_STRUCTURE_TYPE_APPLICATION_INFO, NULL, "nvstats", 1, NULL, 0, VK_API_VERSION_1_3 };
    VkInstanceCreateInfo ici = { VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, NULL, 0, &ai };
    VkInstance inst; CHK(vkCreateInstance(&ici, NULL, &inst)); load_inst(inst);
    uint32_t np = 8; VkPhysicalDevice pds[8]; CHK(vkEnumeratePhysicalDevices(inst, &np, pds));
    VkPhysicalDevice pd = VK_NULL_HANDLE; VkPhysicalDeviceProperties pp;
    for (uint32_t i = 0; i < np; i++) { vkGetPhysicalDeviceProperties(pds[i], &pp); if (pp.vendorID == 0x10de || strstr(pp.deviceName, "NVK") || strstr(pp.deviceName, "Helios")) { pd = pds[i]; break; } }
    if (!pd) { fprintf(stderr, "no NVIDIA device\n"); return 1; }
    fprintf(stderr, "device: %s driver %u.%u\n", pp.deviceName, pp.driverVersion >> 22, (pp.driverVersion >> 14) & 0xff);

    // Enable every feature the driver has (vkd3d's SPIR-V uses descriptor indexing, BDA, 16/64-bit, subgroups...).
    VkPhysicalDevicePipelineExecutablePropertiesFeaturesKHR pef = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PIPELINE_EXECUTABLE_PROPERTIES_FEATURES_KHR };
    VkPhysicalDeviceMutableDescriptorTypeFeaturesEXT mdf = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_MUTABLE_DESCRIPTOR_TYPE_FEATURES_EXT, &pef };
    VkPhysicalDeviceVulkan13Features f13 = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES, &mdf };
    VkPhysicalDeviceVulkan12Features f12 = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES, &f13 };
    VkPhysicalDeviceVulkan11Features f11 = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_1_FEATURES, &f12 };
    VkPhysicalDeviceFeatures2 f2 = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2, &f11 };
    vkGetPhysicalDeviceFeatures2(pd, &f2);
    f2.features.robustBufferAccess = VK_FALSE;
    const char *exts[] = { "VK_KHR_pipeline_executable_properties", "VK_EXT_mutable_descriptor_type" };
    float prio = 1; VkDeviceQueueCreateInfo qci = { VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, NULL, 0, 0, 1, &prio };
    VkDeviceCreateInfo dci = { VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, &f2, 0, 1, &qci, 0, NULL, 2, exts, NULL };
    VkDevice dev; CHK(vkCreateDevice(pd, &dci, NULL, &dev));
    PFN_vkGetPipelineExecutablePropertiesKHR gep = (void *)vkGetDeviceProcAddr(dev, "vkGetPipelineExecutablePropertiesKHR");
    PFN_vkGetPipelineExecutableStatisticsKHR ges = (void *)vkGetDeviceProcAddr(dev, "vkGetPipelineExecutableStatisticsKHR");

    int gmode = argc > 2 && (!strcmp(argv[1], "-g") || !strcmp(argv[1], "-v")); int vmode = gmode && !strcmp(argv[1], "-v");
    FILE *gl = gmode ? fopen(argv[2], "r") : NULL;
    char gline[1024], gfs[512], gvs[512], gkinds[64]; int gdepth = 0;
    for (int fi = gmode ? 3 : 1; gmode ? (fgets(gline, sizeof gline, gl) != NULL) : (fi < argc); fi++) {
        const char *path = argv[fi < argc ? fi : 0];
        if (vmode) { if (sscanf(gline, "%511s %511s %63s", gfs, gvs, gkinds) != 3) continue; path = gfs; }
        else if (gmode) { if (sscanf(gline, "%511s %511s %63s %d", gfs, gvs, gkinds, &gdepth) != 4) continue; path = gfs; }
        spv = rd(path, &nw); if (!spv) continue; nw /= 4;
        uint32_t model = 99, ls[3];
        struct bind b[64]; int nb = reflect(b, 64, &model, ls);
        const char *name = strrchr(path, '/'); name = name ? name + 1 : path;
        if (vmode ? model != 0 /* Vertex */ : gmode ? model != 4 /* Fragment */ : model != 5 /* GLCompute */) { free(spv); continue; }
        // One set layout per set index used (0..7), mutable where several types alias one binding.
        VkDescriptorSetLayout sl[8] = {0}; uint32_t maxset = 0;
        for (int k = 0; k < nb; k++) if (b[k].set + 1 > maxset) maxset = b[k].set + 1;
        int bad = 0;
        for (uint32_t s = 0; s < maxset && !bad; s++) {
            VkDescriptorSetLayoutBinding lb[64]; VkDescriptorBindingFlags fl[64]; VkMutableDescriptorTypeListEXT ml[64]; int nl = 0;
            for (int k = 0; k < nb; k++) if (b[k].set == s) {
                int mut = b[k].ntypes > 1;
                lb[nl] = (VkDescriptorSetLayoutBinding){ b[k].binding, mut ? VK_DESCRIPTOR_TYPE_MUTABLE_EXT : b[k].types[0], b[k].count, VK_SHADER_STAGE_ALL, NULL };
                ml[nl] = (VkMutableDescriptorTypeListEXT){ mut ? (uint32_t)b[k].ntypes : 0, mut ? b[k].types : NULL };
                fl[nl] = VK_DESCRIPTOR_BINDING_PARTIALLY_BOUND_BIT | (b[k].types[0] == VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER && !mut ? 0 : VK_DESCRIPTOR_BINDING_UPDATE_AFTER_BIND_BIT);
                nl++;
            }
            VkMutableDescriptorTypeCreateInfoEXT mci = { VK_STRUCTURE_TYPE_MUTABLE_DESCRIPTOR_TYPE_CREATE_INFO_EXT, NULL, (uint32_t)nl, ml };
            VkDescriptorSetLayoutBindingFlagsCreateInfo bfi = { VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_BINDING_FLAGS_CREATE_INFO, &mci, (uint32_t)nl, fl };
            VkDescriptorSetLayoutCreateInfo lci = { VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO, &bfi, VK_DESCRIPTOR_SET_LAYOUT_CREATE_UPDATE_AFTER_BIND_POOL_BIT, (uint32_t)nl, lb };
            if (vkCreateDescriptorSetLayout(dev, &lci, NULL, &sl[s]) != VK_SUCCESS) bad = 1;
        }
        VkPushConstantRange pcr = { VK_SHADER_STAGE_ALL, 0, 256 };
        VkPipelineLayoutCreateInfo plci = { VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO, NULL, 0, maxset, sl, 1, &pcr };
        VkPipelineLayout pl = VK_NULL_HANDLE; if (!bad && vkCreatePipelineLayout(dev, &plci, NULL, &pl) != VK_SUCCESS) bad = 2;
        VkShaderModuleCreateInfo smci = { VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO, NULL, 0, nw * 4, spv };
        VkShaderModule sm = VK_NULL_HANDLE; if (!bad && vkCreateShaderModule(dev, &smci, NULL, &sm) != VK_SUCCESS) bad = 3;
        VkPipeline pipe = VK_NULL_HANDLE;
        if (!bad && gmode) {
            VkShaderModule vsm = mkmod(dev, gvs);
            VkFormat cf[8]; uint32_t ncf = 0;
            VkVertexInputAttributeDescription va[32]; uint32_t nva = 0;
            VkVertexInputBindingDescription vb = { 0, 256, VK_VERTEX_INPUT_RATE_VERTEX };
            if (vmode) {
                cf[ncf++] = VK_FORMAT_R32G32B32A32_SFLOAT;
                for (char *t = strtok(strcmp(gkinds, "-") ? gkinds : "", ","); t && nva < 32; t = strtok(NULL, ",")) {
                    unsigned loc; char k; unsigned n; if (sscanf(t, "%u:%c%u", &loc, &k, &n) != 3) continue;
                    static const VkFormat ff[] = { VK_FORMAT_R32_SFLOAT, VK_FORMAT_R32G32_SFLOAT, VK_FORMAT_R32G32B32_SFLOAT, VK_FORMAT_R32G32B32A32_SFLOAT };
                    static const VkFormat fi[] = { VK_FORMAT_R32_SINT, VK_FORMAT_R32G32_SINT, VK_FORMAT_R32G32B32_SINT, VK_FORMAT_R32G32B32A32_SINT };
                    static const VkFormat fu[] = { VK_FORMAT_R32_UINT, VK_FORMAT_R32G32_UINT, VK_FORMAT_R32G32B32_UINT, VK_FORMAT_R32G32B32A32_UINT };
                    va[nva] = (VkVertexInputAttributeDescription){ loc, 0, (k == 'f' ? ff : k == 'i' ? fi : fu)[n - 1], loc * 16 }; nva++;
                }
            }
            for (const char *k = gkinds; !vmode && *k && ncf < 8 && strcmp(gkinds, "-"); k++)
                cf[ncf++] = *k == 'f' ? VK_FORMAT_R32G32B32A32_SFLOAT : *k == 'i' ? VK_FORMAT_R32G32B32A32_SINT : *k == 'u' ? VK_FORMAT_R32G32B32A32_UINT : VK_FORMAT_R8G8B8A8_UNORM;
            VkPipelineRenderingCreateInfo rci = { VK_STRUCTURE_TYPE_PIPELINE_RENDERING_CREATE_INFO, NULL, 0, ncf, cf, VK_FORMAT_D32_SFLOAT, VK_FORMAT_UNDEFINED };
            VkPipelineShaderStageCreateInfo st[2] = {
                { VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, NULL, 0, VK_SHADER_STAGE_VERTEX_BIT, vmode ? sm : vsm, "main", NULL },
                { VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, NULL, 0, VK_SHADER_STAGE_FRAGMENT_BIT, vmode ? vsm : sm, "main", NULL } };
            VkPipelineVertexInputStateCreateInfo vi = { VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO, NULL, 0, nva ? 1u : 0u, &vb, nva, va };
            VkPipelineInputAssemblyStateCreateInfo ia = { VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO, NULL, 0, VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST };
            VkPipelineViewportStateCreateInfo vp = { VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO, NULL, 0, 1, NULL, 1, NULL };
            VkPipelineRasterizationStateCreateInfo rs = { VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO }; rs.lineWidth = 1;
            VkPipelineMultisampleStateCreateInfo ms = { VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO }; ms.rasterizationSamples = VK_SAMPLE_COUNT_1_BIT;
            VkPipelineDepthStencilStateCreateInfo ds = { VK_STRUCTURE_TYPE_PIPELINE_DEPTH_STENCIL_STATE_CREATE_INFO };
            VkPipelineColorBlendAttachmentState cba[8]; memset(cba, 0, sizeof cba); for (int i = 0; i < 8; i++) cba[i].colorWriteMask = 0xf;
            VkPipelineColorBlendStateCreateInfo cb = { VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO, NULL, 0, 0, 0, ncf, cba };
            VkDynamicState dyn[2] = { VK_DYNAMIC_STATE_VIEWPORT, VK_DYNAMIC_STATE_SCISSOR };
            VkPipelineDynamicStateCreateInfo dy = { VK_STRUCTURE_TYPE_PIPELINE_DYNAMIC_STATE_CREATE_INFO, NULL, 0, 2, dyn };
            VkGraphicsPipelineCreateInfo gp = { VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO, &rci, VK_PIPELINE_CREATE_CAPTURE_STATISTICS_BIT_KHR,
                2, st, &vi, &ia, NULL, &vp, &rs, &ms, &ds, &cb, &dy, pl, VK_NULL_HANDLE, 0, VK_NULL_HANDLE, 0 };
            if (!vsm || vkCreateGraphicsPipelines(dev, VK_NULL_HANDLE, 1, &gp, NULL, &pipe) != VK_SUCCESS) bad = 5;
            if (vsm) vkDestroyShaderModule(dev, vsm, NULL);
        } else if (!bad) {
            VkComputePipelineCreateInfo cpci = { VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO, NULL,
                VK_PIPELINE_CREATE_CAPTURE_STATISTICS_BIT_KHR,
                { VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, NULL, 0, VK_SHADER_STAGE_COMPUTE_BIT, sm, "main", NULL }, pl, VK_NULL_HANDLE, 0 };
            if (vkCreateComputePipelines(dev, VK_NULL_HANDLE, 1, &cpci, NULL, &pipe) != VK_SUCCESS) bad = 4;
        }
        printf("%s local %ux%ux%u", name, ls[0], ls[1], ls[2]);
        if (bad) { printf(" FAILED(%d)\n", bad); }
        else {
            VkPipelineInfoKHR pi = { VK_STRUCTURE_TYPE_PIPELINE_INFO_KHR, NULL, pipe };
            uint32_t ne = 0; gep(dev, &pi, &ne, NULL);
            VkPipelineExecutablePropertiesKHR ep[4]; for (int i = 0; i < 4; i++) ep[i] = (VkPipelineExecutablePropertiesKHR){ VK_STRUCTURE_TYPE_PIPELINE_EXECUTABLE_PROPERTIES_KHR };
            if (ne > 4) ne = 4; gep(dev, &pi, &ne, ep);
            for (uint32_t e = 0; e < ne; e++) {
                VkPipelineExecutableInfoKHR ei = { VK_STRUCTURE_TYPE_PIPELINE_EXECUTABLE_INFO_KHR, NULL, pipe, e };
                uint32_t ns = 0; ges(dev, &ei, &ns, NULL);
                VkPipelineExecutableStatisticKHR st[64]; if (ns > 64) ns = 64;
                for (uint32_t i = 0; i < ns; i++) st[i] = (VkPipelineExecutableStatisticKHR){ VK_STRUCTURE_TYPE_PIPELINE_EXECUTABLE_STATISTIC_KHR };
                ges(dev, &ei, &ns, st);
                if (gmode) printf(" || [%s]", ep[e].name);
                for (uint32_t i = 0; i < ns; i++) {
                    printf(" | %s=", st[i].name);
                    switch (st[i].format) {
                    case VK_PIPELINE_EXECUTABLE_STATISTIC_FORMAT_BOOL32_KHR: printf("%u", st[i].value.b32); break;
                    case VK_PIPELINE_EXECUTABLE_STATISTIC_FORMAT_INT64_KHR: printf("%lld", (long long)st[i].value.i64); break;
                    case VK_PIPELINE_EXECUTABLE_STATISTIC_FORMAT_UINT64_KHR: printf("%llu", (unsigned long long)st[i].value.u64); break;
                    default: printf("%g", st[i].value.f64);
                    }
                }
            }
            printf("\n");
            vkDestroyPipeline(dev, pipe, NULL);
        }
        if (sm) vkDestroyShaderModule(dev, sm, NULL);
        if (pl) vkDestroyPipelineLayout(dev, pl, NULL);
        for (uint32_t s = 0; s < maxset; s++) if (sl[s]) vkDestroyDescriptorSetLayout(dev, sl[s], NULL);
        memset(dset, 0, MAXID * 4); memset(dbind, 0, MAXID * 4); memset(ty_op, 0, MAXID * 4); memset(var_ty, 0, MAXID * 4);
        memset(bufblock, 0, MAXID * 4); memset(blockdec, 0, MAXID * 4);
        free(spv); fflush(stdout);
    }
    vkDestroyDevice(dev, NULL); vkDestroyInstance(inst, NULL);
    return 0;
}
