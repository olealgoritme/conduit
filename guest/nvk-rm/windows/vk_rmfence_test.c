/*
 * RM fences from NVK (dxvk-on-nvk S4, Mesa patch 0030), the way the Helios
 * D3D11 UMD uses them, without the UMD: loads vulkan_nouveau.dll directly,
 * gets helios_icd_interface_v2 (version 3 entries) and compares
 *
 *  1. completion latency: a submit (empty, or a few ms of fills) followed by
 *     either vkWaitForFences (NVK's CPU wait: spin, then the RM non-stall
 *     event) or queue_rm_fence + rm_fence_wait (an RM semaphore-surface fence
 *     through nvidia-drm on the host and EVENT_REGISTER in the KMD);
 *  2. the copy-engine Present record (helios_icd_interface.h version 6,
 *     queue_rm_fence_v3; guest/windows/docs/rm-copy-engine-present.md 12):
 *     a fill into a dedicated 1920x1080 image, then the fence with the
 *     image's semaphore and source description, checked for the rules the
 *     KMD applies to the 'HEF3' record (value == the fence's, 8-aligned
 *     offset, handles nonzero, the image inside its memory);
 *  3. presenting (optional): N frames of fills into three scanout images,
 *     each shown by scanout_present after a CPU wait (S3) or by
 *     scanout_present_fenced (S4: no wait on the presenting thread), with the
 *     frame pipelining a renderer would use (wait for frame P-2 before reusing
 *     its image).  Reports fps and the presenting thread's time per frame.
 *
 *   vk_rmfence_test.exe [dll] [rounds=200] [fill_MiB=64] [present_seconds=0]
 *
 * Run with NVK_RM=1 and librmclient.dll next to the driver.
 */
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <windows.h>

#define VK_NO_PROTOTYPES
#include <vulkan/vulkan.h>

#include "../../windows/protocol/include/helios_icd_interface.h"

typedef VkResult (VKAPI_PTR *PFN_negotiate)(uint32_t *version);

static PFN_vkGetInstanceProcAddr gipa;
static VkInstance inst;
static VkDevice dev;
static VkPhysicalDevice pd;
static PFN_vkGetDeviceProcAddr gdpa;
static struct helios_icd_api api;
static LARGE_INTEGER qpf;

#define IFN(name) PFN_##name name = (PFN_##name)gipa(inst, #name)
#define DFN(name) PFN_##name name = (PFN_##name)gdpa(dev, #name)

static double now_us(void)
{
    LARGE_INTEGER t;
    QueryPerformanceCounter(&t);
    return (double)t.QuadPart * 1e6 / (double)qpf.QuadPart;
}

static int cmp_d(const void *a, const void *b)
{
    const double x = *(const double *)a, y = *(const double *)b;
    return x < y ? -1 : x > y;
}

static void stats(const char *what, double *v, unsigned n)
{
    if (n == 0)
        return;
    qsort(v, n, sizeof(*v), cmp_d);
    double sum = 0;
    for (unsigned i = 0; i < n; i++)
        sum += v[i];
    printf("  %-46s n=%u min %8.1f  median %8.1f  p90 %8.1f  p99 %8.1f  max %8.1f us\n", what, n,
           v[0], v[n / 2], v[(n * 9) / 10], v[(n * 99) / 100], v[n - 1]);
}

static uint32_t find_type(uint32_t bits, VkMemoryPropertyFlags want)
{
    IFN(vkGetPhysicalDeviceMemoryProperties);
    VkPhysicalDeviceMemoryProperties mp;
    vkGetPhysicalDeviceMemoryProperties(pd, &mp);
    for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
        if ((bits & (1u << i)) && (mp.memoryTypes[i].propertyFlags & want) == want)
            return i;
    return 0;
}

struct img {
    VkImage image;
    VkDeviceMemory mem;
};

static VkResult make_image(uint32_t w, uint32_t h, struct img *out)
{
    DFN(vkCreateImage);
    DFN(vkGetImageMemoryRequirements);
    DFN(vkAllocateMemory);
    DFN(vkBindImageMemory);
    VkImageCreateInfo ici = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
        .imageType = VK_IMAGE_TYPE_2D,
        .format = VK_FORMAT_B8G8R8A8_UNORM,
        .extent = { w, h, 1 },
        .mipLevels = 1,
        .arrayLayers = 1,
        .samples = VK_SAMPLE_COUNT_1_BIT,
        .tiling = VK_IMAGE_TILING_OPTIMAL,
        .usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT,
        .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
    };
    VkResult r = vkCreateImage(dev, &ici, NULL, &out->image);
    if (r)
        return r;
    VkMemoryRequirements req;
    vkGetImageMemoryRequirements(dev, out->image, &req);
    struct helios_export_memory_resource_info hx = {
        .sType = HELIOS_STRUCTURE_TYPE_EXPORT_MEMORY_RESOURCE_INFO,
    };
    VkMemoryDedicatedAllocateInfo ded = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
        .pNext = &hx,
        .image = out->image,
    };
    VkMemoryAllocateInfo mai = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        .pNext = &ded,
        .allocationSize = req.size,
        .memoryTypeIndex = find_type(req.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT),
    };
    r = vkAllocateMemory(dev, &mai, NULL, &out->mem);
    if (r)
        return r;
    return vkBindImageMemory(dev, out->image, out->mem, 0);
}

static VkQueue queue;
static VkCommandPool pool;
static VkBuffer buf;
static VkDeviceMemory buf_mem;
static VkDeviceSize buf_size;

static VkCommandBuffer record(int fill, VkImage image, float shade)
{
    DFN(vkAllocateCommandBuffers);
    DFN(vkBeginCommandBuffer);
    DFN(vkCmdFillBuffer);
    DFN(vkCmdPipelineBarrier);
    DFN(vkCmdClearColorImage);
    DFN(vkEndCommandBuffer);
    VkCommandBufferAllocateInfo cai = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
                                        .commandPool = pool,
                                        .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
                                        .commandBufferCount = 1 };
    VkCommandBuffer cb;
    vkAllocateCommandBuffers(dev, &cai, &cb);
    VkCommandBufferBeginInfo bi = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO };
    vkBeginCommandBuffer(cb, &bi);
    if (fill && buf_size)
        vkCmdFillBuffer(cb, buf, 0, buf_size, 0x12345678u);
    if (image != VK_NULL_HANDLE) {
        VkImageSubresourceRange range = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 };
        VkImageMemoryBarrier b = {
            .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
            .dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
            .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
            .newLayout = VK_IMAGE_LAYOUT_GENERAL,
            .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
            .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
            .image = image,
            .subresourceRange = range,
        };
        vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,
                             VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL, 0, NULL, 1, &b);
        VkClearColorValue c = { .float32 = { shade, 0.3f, 1.0f - shade, 1.0f } };
        vkCmdClearColorImage(cb, image, VK_IMAGE_LAYOUT_GENERAL, &c, 1, &range);
    }
    vkEndCommandBuffer(cb);
    return cb;
}

static VkResult submit(VkCommandBuffer cb, VkFence fence)
{
    DFN(vkQueueSubmit);
    VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1,
                        .pCommandBuffers = &cb };
    return vkQueueSubmit(queue, 1, &si, fence);
}

/* One latency series: submit, then wait one way or the other. */
static void latency(const char *what, unsigned rounds, int fill, int use_rm)
{
    DFN(vkCreateFence);
    DFN(vkDestroyFence);
    DFN(vkWaitForFences);
    DFN(vkResetFences);
    DFN(vkQueueWaitIdle);
    VkCommandBuffer cb = record(fill, VK_NULL_HANDLE, 0);
    VkFenceCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
    VkFence f;
    vkCreateFence(dev, &fci, NULL, &f);
    double *t = calloc(rounds, sizeof(double));
    double *tc = calloc(rounds, sizeof(double));
    DFN(vkGetFenceStatus);
    unsigned n = 0, errors = 0, early = 0;
    for (unsigned i = 0; i < rounds; i++) {
        vkResetFences(dev, 1, &f);
        const double a = now_us();
        if (submit(cb, f)) {
            errors++;
            break;
        }
        if (use_rm) {
            uint32_t fence = 0;
            uint64_t value = 0;
            const double c0 = now_us();
            VkResult r = api.queue_rm_fence(dev, queue, &fence, &value);
            tc[n] = now_us() - c0;
            if (r != VK_SUCCESS) {
                printf("  queue_rm_fence: %d\n", r);
                errors++;
                break;
            }
            r = api.rm_fence_wait(dev, fence, 5000000000ull);
            t[n] = now_us() - a;
            /* The submit's own fence is released before the timeline value
             * on the same channel: it must read signalled by now. */
            if (vkGetFenceStatus(dev, f) != VK_SUCCESS)
                early++;
            api.rm_fence_close(dev, fence);
            if (r != VK_SUCCESS)
                errors++;
            vkWaitForFences(dev, 1, &f, VK_TRUE, 5000000000ull);
        } else {
            VkResult r = vkWaitForFences(dev, 1, &f, VK_TRUE, 5000000000ull);
            t[n] = now_us() - a;
            if (r != VK_SUCCESS)
                errors++;
        }
        n++;
    }
    vkQueueWaitIdle(queue);
    char label[160];
    if (early)
        printf("  %s: %u wakes BEFORE the submit's fence signalled\n", what, early);
    snprintf(label, sizeof(label), "%s: submit -> wake%s", what, errors ? " (ERRORS)" : "");
    stats(label, t, n);
    if (use_rm) {
        snprintf(label, sizeof(label), "%s: queue_rm_fence call", what);
        stats(label, tc, n);
    }
    free(t);
    free(tc);
    vkDestroyFence(dev, f, NULL);
}

/* queue_rm_fence_v3 for a dedicated image: the record's fields as the KMD
 * checks them.  Returns the number of faults. */
static unsigned copy_record_check(void)
{
    DFN(vkDestroyImage);
    DFN(vkFreeMemory);
    DFN(vkFreeCommandBuffers);
    DFN(vkQueueWaitIdle);
    if (api.size < offsetof(struct helios_icd_api, queue_rm_fence_v3) + sizeof(void *) ||
        !api.queue_rm_fence_v3) {
        printf("copy record: no queue_rm_fence_v3 in this ICD (version %u)\n", api.version);
        return 0;
    }
    struct img im;
    if (make_image(1920, 1080, &im)) {
        printf("copy record: image failed\n");
        return 1;
    }
    VkCommandBuffer cb = record(1, im.image, 0.5f);
    submit(cb, VK_NULL_HANDLE);

    unsigned faults = 0;
    uint32_t fence = 0;
    uint64_t value = 0;
    struct helios_icd_rm_copy copy;
    VkResult r = api.queue_rm_fence_v3(dev, queue, im.mem, im.image, &fence, &value, &copy);
    const struct helios_icd_rm_semaphore *s = &copy.semaphore;
    const struct helios_icd_rm_source *src = &copy.source;
    printf("copy record: %d, fence %u value %llu\n"
           "  semaphore client 0x%x memory 0x%x offset %llu value %llu\n"
           "  source client 0x%x memory 0x%x offset %llu size %llu modifier 0x%016llx\n"
           "         %ux%u pitch %u fourcc 0x%08x flags 0x%x\n",
           r, fence, (unsigned long long)value, s->h_client, s->h_memory,
           (unsigned long long)s->offset, (unsigned long long)s->value, src->h_client,
           src->h_memory, (unsigned long long)src->offset, (unsigned long long)src->size,
           (unsigned long long)src->modifier, src->width, src->height, src->pitch, src->fourcc,
           src->flags);
    if (r == VK_SUCCESS) {
        const uint64_t rows = src->modifier == 0 ? src->height
            : ((src->height + (8u << (src->modifier & 0xf)) - 1) / (8u << (src->modifier & 0xf))) *
              (8u << (src->modifier & 0xf));
#define FAULT(cond, what) do { if (cond) { printf("  FAULT: %s\n", what); faults++; } } while (0)
        FAULT(fence == 0, "no fence");
        FAULT(s->value != value || value == 0, "semaphore value is not the fence's");
        FAULT(s->h_client == 0 || s->h_memory == 0, "semaphore handle zero");
        FAULT(s->offset % 8 != 0 || s->offset + 8 > 4096, "semaphore offset");
        FAULT(src->h_client != s->h_client, "source client differs from the semaphore's");
        FAULT(src->h_memory == 0 || src->h_memory == s->h_memory, "source memory");
        FAULT(src->width != 1920 || src->height != 1080, "source size");
        FAULT(src->fourcc != HELIOS_DRM_FORMAT_ARGB8888, "source fourcc (B8G8R8A8 = AR24)");
        FAULT(src->pitch < 1920 * 4, "source pitch");
        FAULT(src->modifier != 0 && (src->modifier & ~0xfull) !=
              (HELIOS_DRM_FORMAT_MOD_NVIDIA_BL_GB20X & ~0xfull), "source modifier family");
        FAULT(src->offset + (uint64_t)src->pitch * rows > src->size, "source past its memory");
        FAULT(src->flags != 0 || src->reserved != 0, "source flags");
#undef FAULT
    } else if (r == VK_INCOMPLETE) {
        printf("  the image could not be described (fence only)\n");
        faults++;
    } else {
        faults++;
    }
    if (fence != 0) {
        if (api.rm_fence_wait(dev, fence, 5000000000ull) != VK_SUCCESS) {
            printf("  FAULT: the fence did not fire\n");
            faults++;
        }
        api.rm_fence_close(dev, fence);
    }
    /* A null image: the fence alone (VK_INCOMPLETE), the record all zero */
    fence = 0;
    memset(&copy, 0xff, sizeof(copy));
    r = api.queue_rm_fence_v3(dev, queue, VK_NULL_HANDLE, VK_NULL_HANDLE, &fence, NULL, &copy);
    const struct helios_icd_rm_copy zero = { 0 };
    if (r != VK_INCOMPLETE || fence == 0 || memcmp(&copy, &zero, sizeof(copy)) != 0) {
        printf("  FAULT: no image: %d, fence %u, record %s\n", r, fence,
               memcmp(&copy, &zero, sizeof(copy)) ? "not zero" : "zero");
        faults++;
    }
    if (fence != 0) {
        api.rm_fence_wait(dev, fence, 5000000000ull);
        api.rm_fence_close(dev, fence);
    }
    vkQueueWaitIdle(queue);
    vkFreeCommandBuffers(dev, pool, 1, &cb);
    vkDestroyImage(dev, im.image, NULL);
    vkFreeMemory(dev, im.mem, NULL);
    printf("copy record: %s\n", faults ? "FAIL" : "PASS");
    return faults;
}

static void present_run(int fenced, unsigned seconds, int fill)
{
    DFN(vkCreateFence);
    DFN(vkDestroyFence);
    DFN(vkWaitForFences);
    DFN(vkResetFences);
    DFN(vkQueueWaitIdle);
    DFN(vkFreeCommandBuffers);
    enum { N = 3 };
    struct img im[N];
    for (int i = 0; i < N; i++) {
        if (make_image(1920, 1080, &im[i])) {
            printf("  image %d failed\n", i);
            return;
        }
    }
    VkFenceCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO,
                              .flags = VK_FENCE_CREATE_SIGNALED_BIT };
    VkFence frame_fence[N];
    VkCommandBuffer cbs[N] = { 0 };
    for (int i = 0; i < N; i++)
        vkCreateFence(dev, &fci, NULL, &frame_fence[i]);

    unsigned frames = 0, failed = 0;
    double present_us = 0, wait_us = 0;
    const double t_end = now_us() + seconds * 1e6, t0 = now_us();
    while (now_us() < t_end) {
        const int k = frames % N;
        /* Reuse rule (rm-fence-marker.md): image k was presented N frames ago;
         * frame k+1's fence must have fired. Waiting for k's own frame fence
         * one frame later than that is the renderer's usual pipelining. */
        const double w0 = now_us();
        vkWaitForFences(dev, 1, &frame_fence[(frames + 1) % N], VK_TRUE, 5000000000ull);
        wait_us += now_us() - w0;
        vkResetFences(dev, 1, &frame_fence[k]);
        if (cbs[k])
            vkFreeCommandBuffers(dev, pool, 1, &cbs[k]);
        cbs[k] = record(fill, im[k].image, (float)(frames % 60) / 60.0f);
        submit(cbs[k], frame_fence[k]);

        const double p0 = now_us();
        VkResult r;
        if (fenced) {
            uint32_t fence = 0;
            r = api.queue_rm_fence(dev, queue, &fence, NULL);
            if (r == VK_SUCCESS)
                r = api.scanout_present_fenced(dev, im[k].mem, im[k].image, fence);
        } else {
            vkWaitForFences(dev, 1, &frame_fence[k], VK_TRUE, 5000000000ull);
            r = api.scanout_present(dev, im[k].mem, im[k].image);
        }
        present_us += now_us() - p0;
        if (r != VK_SUCCESS)
            failed++;
        frames++;
    }
    vkQueueWaitIdle(queue);
    const double secs = (now_us() - t0) / 1e6;
    api.scanout_release(dev);
    printf("  %-10s %u frames in %.2f s = %.1f fps, %u failed; presenting thread %.1f us/frame "
           "in present, %.1f us/frame waiting for frame P-2\n",
           fenced ? "fenced" : "cpu-wait", frames, secs, frames / secs, failed,
           frames ? present_us / frames : 0, frames ? wait_us / frames : 0);
    DFN(vkDestroyImage);
    DFN(vkFreeMemory);
    for (int i = 0; i < N; i++) {
        vkDestroyFence(dev, frame_fence[i], NULL);
        vkDestroyImage(dev, im[i].image, NULL);
        vkFreeMemory(dev, im[i].mem, NULL);
    }
}

int main(int argc, char **argv)
{
    const char *path = argc > 1 ? argv[1] : "vulkan_nouveau.dll";
    const unsigned rounds = argc > 2 ? (unsigned)atoi(argv[2]) : 200;
    const unsigned fill_mib = argc > 3 ? (unsigned)atoi(argv[3]) : 64;
    const unsigned present_s = argc > 4 ? (unsigned)atoi(argv[4]) : 0;
    QueryPerformanceFrequency(&qpf);

    HMODULE dll = LoadLibraryExA(path, NULL, LOAD_WITH_ALTERED_SEARCH_PATH);
    if (!dll) {
        printf("LoadLibrary(%s) failed: %lu\n", path, (unsigned long)GetLastError());
        return 1;
    }
    PFN_negotiate negotiate =
        (PFN_negotiate)(void *)GetProcAddress(dll, "vk_icdNegotiateLoaderICDInterfaceVersion");
    gipa = (PFN_vkGetInstanceProcAddr)(void *)GetProcAddress(dll, "vk_icdGetInstanceProcAddr");
    PFN_helios_icd_interface_v2 helios =
        (PFN_helios_icd_interface_v2)(void *)GetProcAddress(dll, HELIOS_ICD_INTERFACE_EXPORT);
    if (!negotiate || !gipa || !helios) {
        printf("missing entry points\n");
        return 1;
    }
    uint32_t version = 7;
    negotiate(&version);

    api.size = sizeof(api);
    VkResult r = helios(HELIOS_ICD_INTERFACE_VERSION, &api);
    printf("helios_icd_interface_v2: %d, version %u size %u (want %u) caps 0x%x: rm_fence %d, "
           "scanout_fence_kmd %d, present_fence_kmd %d, scanout %d\n",
           r, api.version, api.size, (unsigned)sizeof(api), api.caps,
           !!(api.caps & HELIOS_ICD_CAP_RM_FENCE), !!(api.caps & HELIOS_ICD_CAP_SCANOUT_FENCE_KMD),
           !!(api.caps & HELIOS_ICD_CAP_PRESENT_FENCE_KMD), !!(api.caps & HELIOS_ICD_CAP_SCANOUT));
    /* Version 3 is enough for the latency series (an older ICD fills less) */
    if (r || api.size < offsetof(struct helios_icd_api, memory_res_plane1) || !api.queue_rm_fence ||
        !(api.caps & HELIOS_ICD_CAP_RM_FENCE)) {
        printf("no RM fences in this ICD\n");
        return 1;
    }

    PFN_vkCreateInstance vkCreateInstance =
        (PFN_vkCreateInstance)gipa(VK_NULL_HANDLE, "vkCreateInstance");
    VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
                              .pApplicationName = "vk_rmfence_test",
                              .apiVersion = VK_API_VERSION_1_3 };
    VkInstanceCreateInfo ci = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                                .pApplicationInfo = &app };
    if (vkCreateInstance(&ci, NULL, &inst)) {
        printf("vkCreateInstance failed\n");
        return 1;
    }
    IFN(vkEnumeratePhysicalDevices);
    IFN(vkCreateDevice);
    gdpa = (PFN_vkGetDeviceProcAddr)gipa(inst, "vkGetDeviceProcAddr");
    uint32_t n = 1;
    r = vkEnumeratePhysicalDevices(inst, &n, &pd);
    if ((r != VK_SUCCESS && r != VK_INCOMPLETE) || n == 0) {
        printf("no physical device\n");
        return 1;
    }
    float prio = 1.0f;
    VkDeviceQueueCreateInfo qci = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                                    .queueFamilyIndex = 0, .queueCount = 1,
                                    .pQueuePriorities = &prio };
    VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                               .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci };
    if (vkCreateDevice(pd, &dci, NULL, &dev)) {
        printf("vkCreateDevice failed\n");
        return 1;
    }
    DFN(vkGetDeviceQueue);
    DFN(vkCreateCommandPool);
    DFN(vkCreateBuffer);
    DFN(vkGetBufferMemoryRequirements);
    DFN(vkAllocateMemory);
    DFN(vkBindBufferMemory);
    vkGetDeviceQueue(dev, 0, 0, &queue);
    VkCommandPoolCreateInfo pci = { .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
                                    .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
                                    .queueFamilyIndex = 0 };
    vkCreateCommandPool(dev, &pci, NULL, &pool);
    if (fill_mib) {
        buf_size = (VkDeviceSize)fill_mib << 20;
        VkBufferCreateInfo bci = { .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
                                   .size = buf_size,
                                   .usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT };
        vkCreateBuffer(dev, &bci, NULL, &buf);
        VkMemoryRequirements req;
        vkGetBufferMemoryRequirements(dev, buf, &req);
        VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                                     .allocationSize = req.size,
                                     .memoryTypeIndex = find_type(req.memoryTypeBits,
                                                                  VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT) };
        if (vkAllocateMemory(dev, &mai, NULL, &buf_mem) ||
            vkBindBufferMemory(dev, buf, buf_mem, 0)) {
            printf("fill buffer allocation failed\n");
            buf_size = 0;
        }
    }

    printf("completion latency, %u rounds each (alternating series):\n", rounds);
    latency("empty submit, vkWaitForFences", rounds, 0, 0);
    latency("empty submit, RM fence", rounds, 0, 1);
    char what[96];
    snprintf(what, sizeof(what), "%u MiB fill, vkWaitForFences", fill_mib);
    latency(what, rounds, 1, 0);
    snprintf(what, sizeof(what), "%u MiB fill, RM fence", fill_mib);
    latency(what, rounds, 1, 1);

    const unsigned copy_faults = copy_record_check();

    if (present_s > 0) {
        printf("presenting 1920x1080 on scanout 0, 3 images, %u MiB fill per frame:\n", fill_mib);
        present_run(0, present_s, 1);
        present_run(1, present_s, 1);
    }

    DFN(vkDestroyDevice);
    vkDestroyDevice(dev, NULL);
    printf("done\n");
    return copy_faults ? 2 : 0;
}
