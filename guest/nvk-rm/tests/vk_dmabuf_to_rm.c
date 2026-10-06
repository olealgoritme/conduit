/* SPDX-License-Identifier: MIT */
/*
 * vk_dmabuf_to_rm: can memory NVIDIA's own Vulkan driver allocated become RM
 * memory of another RM client?  (Linux host only; spike X4 of
 * guest/windows/docs/shared-surfaces.md, the reverse direction a DWM on NVK
 * needs for surfaces Venus processes made.)
 *
 * Allocates exportable memory with the host's Vulkan driver (the first
 * NVIDIA device), VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT (or
 * OPAQUE_FD with `opaque`), writes the coordinate pattern of rm_export_exec's
 * LINEAR layout through a CPU mapping, exports the fd and runs CMD with it
 * described as rm_export_exec describes its dma-buf (RM_DMABUF_FD, RM_SIZE,
 * RM_MODIFIER = 0, RM_PITCH, RM_W, RM_H, RM_IMAGE_SIZE). With
 * rm_reimport_check as CMD: PRIME_FD_TO_HANDLE on the render node,
 * DRM_NVIDIA_GEM_EXPORT_NVKMS_MEMORY, OS_UNIX_IMPORT_OBJECT_FROM_FD, CPU map,
 * check.
 *
 * Build:
 *   cc -std=gnu11 -O2 -Wall vk_dmabuf_to_rm.c -lvulkan -o vk_dmabuf_to_rm
 * Run:
 *   vk_dmabuf_to_rm [dmabuf|opaque] guest/rmclient/build-make/rm_reimport_check
 */
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>
#include <vulkan/vulkan.h>

#define W 1920u
#define H 1080u

#define CHECK(x)                                                                 \
    do {                                                                         \
        VkResult r_ = (x);                                                       \
        if (r_ != VK_SUCCESS) {                                                  \
            fprintf(stderr, "[FAIL] %s: %d\n", #x, r_);                          \
            return 1;                                                            \
        }                                                                        \
    } while (0)

int main(int argc, char **argv)
{
    if (argc < 3) {
        fprintf(stderr, "usage: %s dmabuf|opaque CMD [ARGS]\n", argv[0]);
        return 2;
    }
    const VkExternalMemoryHandleTypeFlagBits type = !strcmp(argv[1], "opaque")
        ? VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT
        : VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT;

    VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
                              .apiVersion = VK_API_VERSION_1_2 };
    VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                                 .pApplicationInfo = &app };
    VkInstance inst;
    CHECK(vkCreateInstance(&ici, NULL, &inst));
    uint32_t n = 8;
    VkPhysicalDevice pds[8];
    CHECK(vkEnumeratePhysicalDevices(inst, &n, pds));
    VkPhysicalDevice pd = VK_NULL_HANDLE;
    for (uint32_t i = 0; i < n && !pd; i++) {
        VkPhysicalDeviceProperties p;
        vkGetPhysicalDeviceProperties(pds[i], &p);
        if (p.vendorID == 0x10de) {
            pd = pds[i];
            printf("       vk: %s, driver 0x%x\n", p.deviceName, p.driverVersion);
        }
    }
    if (!pd)
        return 77;

    const char *exts[] = { VK_KHR_EXTERNAL_MEMORY_FD_EXTENSION_NAME,
                           VK_EXT_EXTERNAL_MEMORY_DMA_BUF_EXTENSION_NAME };
    float prio = 1.0f;
    VkDeviceQueueCreateInfo qci = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                                    .queueCount = 1, .pQueuePriorities = &prio };
    VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                               .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci,
                               .enabledExtensionCount = 2, .ppEnabledExtensionNames = exts };
    VkDevice dev;
    CHECK(vkCreateDevice(pd, &dci, NULL, &dev));

    const VkDeviceSize size = (VkDeviceSize)W * 4 * H;
    VkExternalMemoryBufferCreateInfo ebi = {
        .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_BUFFER_CREATE_INFO, .handleTypes = type };
    VkBufferCreateInfo bci = { .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .pNext = &ebi,
                               .size = size, .usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT |
                                                      VK_BUFFER_USAGE_TRANSFER_DST_BIT };
    VkBuffer buf;
    CHECK(vkCreateBuffer(dev, &bci, NULL, &buf));
    VkMemoryRequirements req;
    vkGetBufferMemoryRequirements(dev, buf, &req);
    VkPhysicalDeviceMemoryProperties mp;
    vkGetPhysicalDeviceMemoryProperties(pd, &mp);
    uint32_t mt = UINT32_MAX;
    const VkMemoryPropertyFlags want = VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT |
                                       VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT;
    for (uint32_t i = 0; i < mp.memoryTypeCount && mt == UINT32_MAX; i++)
        if ((req.memoryTypeBits & (1u << i)) && (mp.memoryTypes[i].propertyFlags & want) == want)
            mt = i;
    const int staged = mt == UINT32_MAX;
    for (uint32_t i = 0; i < mp.memoryTypeCount && mt == UINT32_MAX; i++)
        if ((req.memoryTypeBits & (1u << i)) &&
            (mp.memoryTypes[i].propertyFlags & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT))
            mt = i;
    if (mt == UINT32_MAX) {
        fprintf(stderr, "[FAIL] no device-local type for exportable memory\n");
        return 1;
    }
    VkExportMemoryAllocateInfo exi = { .sType = VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO,
                                       .handleTypes = type };
    VkMemoryDedicatedAllocateInfo ded = { .sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
                                          .pNext = &exi, .buffer = buf };
    VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .pNext = &ded,
                                 .allocationSize = req.size, .memoryTypeIndex = mt };
    VkDeviceMemory mem;
    CHECK(vkAllocateMemory(dev, &mai, NULL, &mem));
    CHECK(vkBindBufferMemory(dev, buf, mem, 0));

    /* The pattern: through a mapping, or a host-visible staging buffer and a
     * GPU copy when the exportable memory is device-local only. */
    VkBuffer src = buf;
    VkDeviceMemory srcmem = mem;
    if (staged) {
        VkBufferCreateInfo sbi = { .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size = size,
                                   .usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT };
        CHECK(vkCreateBuffer(dev, &sbi, NULL, &src));
        VkMemoryRequirements sreq;
        vkGetBufferMemoryRequirements(dev, src, &sreq);
        uint32_t smt = UINT32_MAX;
        for (uint32_t i = 0; i < mp.memoryTypeCount && smt == UINT32_MAX; i++)
            if ((sreq.memoryTypeBits & (1u << i)) &&
                (mp.memoryTypes[i].propertyFlags & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT) &&
                (mp.memoryTypes[i].propertyFlags & VK_MEMORY_PROPERTY_HOST_COHERENT_BIT))
                smt = i;
        VkMemoryAllocateInfo smai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                                      .allocationSize = sreq.size, .memoryTypeIndex = smt };
        CHECK(vkAllocateMemory(dev, &smai, NULL, &srcmem));
        CHECK(vkBindBufferMemory(dev, src, srcmem, 0));
    }
    uint32_t *p;
    CHECK(vkMapMemory(dev, srcmem, 0, VK_WHOLE_SIZE, 0, (void **)&p));
    for (uint32_t y = 0; y < H; y++)
        for (uint32_t x = 0; x < W; x++)
            p[(size_t)y * W + x] = (y << 16) | x;
    vkUnmapMemory(dev, srcmem);
    if (staged) {
        VkQueue q;
        vkGetDeviceQueue(dev, 0, 0, &q);
        VkCommandPoolCreateInfo pci = { .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO };
        VkCommandPool pool;
        CHECK(vkCreateCommandPool(dev, &pci, NULL, &pool));
        VkCommandBufferAllocateInfo cai = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
                                            .commandPool = pool,
                                            .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
                                            .commandBufferCount = 1 };
        VkCommandBuffer cb;
        CHECK(vkAllocateCommandBuffers(dev, &cai, &cb));
        VkCommandBufferBeginInfo bi = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO };
        CHECK(vkBeginCommandBuffer(cb, &bi));
        VkBufferCopy region = { 0, 0, size };
        vkCmdCopyBuffer(cb, src, buf, 1, &region);
        CHECK(vkEndCommandBuffer(cb));
        VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1,
                            .pCommandBuffers = &cb };
        CHECK(vkQueueSubmit(q, 1, &si, VK_NULL_HANDLE));
        CHECK(vkQueueWaitIdle(q));
        vkDestroyCommandPool(dev, pool, NULL);
    }
    printf("[ ok ] vk: %llu B of exportable memory (type %u, %s), pattern written\n",
           (unsigned long long)req.size, mt, staged ? "device-local, GPU copy" : "mapped");

    PFN_vkGetMemoryFdKHR getfd = (PFN_vkGetMemoryFdKHR)vkGetDeviceProcAddr(dev, "vkGetMemoryFdKHR");
    VkMemoryGetFdInfoKHR gfi = { .sType = VK_STRUCTURE_TYPE_MEMORY_GET_FD_INFO_KHR,
                                 .memory = mem, .handleType = type };
    int fd = -1;
    CHECK(getfd(dev, &gfi, &fd));
    off_t fsz = lseek(fd, 0, SEEK_END);
    lseek(fd, 0, SEEK_SET);
    printf("[ ok ] vk: exported as %s fd %d (lseek size %lld)\n", argv[1], fd, (long long)fsz);

    char b[64];
    snprintf(b, sizeof b, "%d", fd);
    setenv("RM_DMABUF_FD", b, 1);
    snprintf(b, sizeof b, "%llu", (unsigned long long)(fsz > 0 ? (unsigned long long)fsz : req.size));
    setenv("RM_SIZE", b, 1);
    setenv("RM_MODIFIER", "0", 1);
    snprintf(b, sizeof b, "%u", W * 4);
    setenv("RM_PITCH", b, 1);
    snprintf(b, sizeof b, "%llu", (unsigned long long)size);
    setenv("RM_IMAGE_SIZE", b, 1);
    snprintf(b, sizeof b, "%u", W);
    setenv("RM_W", b, 1);
    snprintf(b, sizeof b, "%u", H);
    setenv("RM_H", b, 1);
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) {
        execvp(argv[2], argv + 2);
        _exit(127);
    }
    int st = 0;
    waitpid(pid, &st, 0);
    close(fd);
    vkFreeMemory(dev, mem, NULL);
    vkDestroyBuffer(dev, buf, NULL);
    vkDestroyDevice(dev, NULL);
    vkDestroyInstance(inst, NULL);
    return WIFEXITED(st) ? WEXITSTATUS(st) : 1;
}
