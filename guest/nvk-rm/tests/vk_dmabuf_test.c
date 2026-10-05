/* dma-buf round trip between two VkDevices (two RM clients under NVK-on-RM):
 * device A fills a device-local exportable buffer (staging copy of
 * data[i] = i * 3 + 7) and exports it as a dma-buf; device B imports the
 * dma-buf, copies it into a host-visible buffer and checks every value.
 * Also checks that two exports of one memory are the same dma-buf.
 *
 *   vk_dmabuf_test [N]
 */
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <sys/stat.h>
#include <unistd.h>

#define CHECK(x) do { VkResult r_ = (x); \
   if (r_ != VK_SUCCESS) { fprintf(stderr, "FAIL %s:%d: %s -> %d\n", __FILE__, __LINE__, #x, r_); exit(1); } \
   } while (0)

struct dev {
   VkDevice dev;
   VkQueue queue;
   VkCommandPool pool;
};

static VkPhysicalDeviceMemoryProperties mp;

static uint32_t find_mem(uint32_t bits, VkMemoryPropertyFlags want)
{
   for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
      if ((bits & (1u << i)) && (mp.memoryTypes[i].propertyFlags & want) == want)
         return i;
   fprintf(stderr, "no memory type for bits 0x%x flags 0x%x\n", bits, want);
   exit(1);
}

static struct dev make_dev(VkPhysicalDevice pd)
{
   struct dev d;
   const float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = 0, .queueCount = 1, .pQueuePriorities = &prio };
   const char *exts[] = { "VK_KHR_external_memory_fd", "VK_EXT_external_memory_dma_buf" };
   VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
      .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci,
      .enabledExtensionCount = 2, .ppEnabledExtensionNames = exts };
   CHECK(vkCreateDevice(pd, &dci, NULL, &d.dev));
   vkGetDeviceQueue(d.dev, 0, 0, &d.queue);
   VkCommandPoolCreateInfo pci = { .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
      .queueFamilyIndex = 0 };
   CHECK(vkCreateCommandPool(d.dev, &pci, NULL, &d.pool));
   return d;
}

static void make_buffer(struct dev *d, VkDeviceSize size, VkMemoryPropertyFlags flags,
                        int export_import, int import_fd, VkBuffer *buf, VkDeviceMemory *mem)
{
   VkExternalMemoryBufferCreateInfo ebci = { .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_BUFFER_CREATE_INFO,
      .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT };
   VkBufferCreateInfo bci = { .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
      .pNext = export_import ? &ebci : NULL, .size = size,
      .usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT | VK_BUFFER_USAGE_TRANSFER_DST_BIT,
      .sharingMode = VK_SHARING_MODE_EXCLUSIVE };
   CHECK(vkCreateBuffer(d->dev, &bci, NULL, buf));
   VkMemoryRequirements mr;
   vkGetBufferMemoryRequirements(d->dev, *buf, &mr);

   uint32_t bits = mr.memoryTypeBits;
   VkMemoryDedicatedAllocateInfo ded = { .sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
      .buffer = *buf };
   VkExportMemoryAllocateInfo exp = { .sType = VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO,
      .pNext = &ded, .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT };
   VkImportMemoryFdInfoKHR imp = { .sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR,
      .pNext = &ded, .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT, .fd = import_fd };
   const void *pnext = NULL;
   if (export_import && import_fd >= 0) {
      PFN_vkGetMemoryFdPropertiesKHR props_fn = (PFN_vkGetMemoryFdPropertiesKHR)
         vkGetDeviceProcAddr(d->dev, "vkGetMemoryFdPropertiesKHR");
      VkMemoryFdPropertiesKHR fdp = { .sType = VK_STRUCTURE_TYPE_MEMORY_FD_PROPERTIES_KHR };
      CHECK(props_fn(d->dev, VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT, import_fd, &fdp));
      fprintf(stderr, "  import: memoryTypeBits 0x%x (buffer wants 0x%x)\n", fdp.memoryTypeBits, bits);
      bits &= fdp.memoryTypeBits;
      off_t sz = lseek(import_fd, 0, SEEK_END);
      lseek(import_fd, 0, SEEK_SET);
      if (sz < (off_t)mr.size) { fprintf(stderr, "FAIL dma-buf is %lld bytes\n", (long long)sz); exit(1); }
      mr.size = sz;
      pnext = &imp;
   } else if (export_import) {
      pnext = &exp;
   }
   VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .pNext = pnext,
      .allocationSize = mr.size, .memoryTypeIndex = find_mem(bits, flags) };
   CHECK(vkAllocateMemory(d->dev, &mai, NULL, mem));
   CHECK(vkBindBufferMemory(d->dev, *buf, *mem, 0));
}

static void copy_and_wait(struct dev *d, VkBuffer src, VkBuffer dst, VkDeviceSize size)
{
   VkCommandBufferAllocateInfo cai = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
      .commandPool = d->pool, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1 };
   VkCommandBuffer cb;
   CHECK(vkAllocateCommandBuffers(d->dev, &cai, &cb));
   VkCommandBufferBeginInfo bi = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO };
   CHECK(vkBeginCommandBuffer(cb, &bi));
   VkBufferCopy r = { 0, 0, size };
   vkCmdCopyBuffer(cb, src, dst, 1, &r);
   CHECK(vkEndCommandBuffer(cb));
   VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cb };
   CHECK(vkQueueSubmit(d->queue, 1, &si, VK_NULL_HANDLE));
   CHECK(vkQueueWaitIdle(d->queue));
   vkFreeCommandBuffers(d->dev, d->pool, 1, &cb);
}

int main(int argc, char **argv)
{
   uint32_t n = argc > 1 ? (uint32_t)atoi(argv[1]) : (1u << 20);
   VkDeviceSize size = (VkDeviceSize)n * 4;
   const VkMemoryPropertyFlags host = VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;

   VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .pApplicationName = "vk_dmabuf_test", .apiVersion = VK_API_VERSION_1_3 };
   VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app };
   VkInstance inst;
   CHECK(vkCreateInstance(&ici, NULL, &inst));
   uint32_t npd = 1;
   VkPhysicalDevice pd;
   VkResult r = vkEnumeratePhysicalDevices(inst, &npd, &pd);
   if ((r != VK_SUCCESS && r != VK_INCOMPLETE) || npd == 0) { fprintf(stderr, "no device\n"); return 1; }
   VkPhysicalDeviceProperties props;
   vkGetPhysicalDeviceProperties(pd, &props);
   vkGetPhysicalDeviceMemoryProperties(pd, &mp);
   fprintf(stderr, "device: %s, %u values\n", props.deviceName, n);

   struct dev a = make_dev(pd), b = make_dev(pd);

   /* A: staging -> exportable device-local buffer */
   VkBuffer a_stage, a_buf;
   VkDeviceMemory a_stage_mem, a_mem;
   make_buffer(&a, size, host, 0, -1, &a_stage, &a_stage_mem);
   make_buffer(&a, size, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT, 1, -1, &a_buf, &a_mem);
   uint32_t *p;
   CHECK(vkMapMemory(a.dev, a_stage_mem, 0, size, 0, (void **)&p));
   for (uint32_t i = 0; i < n; i++)
      p[i] = i * 3 + 7;
   vkUnmapMemory(a.dev, a_stage_mem);
   copy_and_wait(&a, a_stage, a_buf, size);

   PFN_vkGetMemoryFdKHR get_fd = (PFN_vkGetMemoryFdKHR)vkGetDeviceProcAddr(a.dev, "vkGetMemoryFdKHR");
   VkMemoryGetFdInfoKHR gfi = { .sType = VK_STRUCTURE_TYPE_MEMORY_GET_FD_INFO_KHR,
      .memory = a_mem, .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT };
   int fd, fd2;
   CHECK(get_fd(a.dev, &gfi, &fd));
   CHECK(get_fd(a.dev, &gfi, &fd2));
   struct stat s1, s2;
   fstat(fd, &s1);
   fstat(fd2, &s2);
   fprintf(stderr, "export: fd %d and %d, same dma-buf: %s\n", fd, fd2,
           s1.st_ino == s2.st_ino ? "yes" : "NO");
   if (s1.st_ino != s2.st_ino) return 1;
   close(fd2);

   /* B: import -> host-visible buffer */
   VkBuffer b_buf, b_read;
   VkDeviceMemory b_mem, b_read_mem;
   make_buffer(&b, size, 0, 1, fd, &b_buf, &b_mem); /* fd now owned by the driver */
   make_buffer(&b, size, host, 0, -1, &b_read, &b_read_mem);
   copy_and_wait(&b, b_buf, b_read, size);
   CHECK(vkMapMemory(b.dev, b_read_mem, 0, size, 0, (void **)&p));
   uint32_t bad = 0;
   for (uint32_t i = 0; i < n; i++)
      if (p[i] != i * 3 + 7 && bad++ < 5)
         fprintf(stderr, "  [%u] = %u, want %u\n", i, p[i], i * 3 + 7);
   vkUnmapMemory(b.dev, b_read_mem);

   vkDestroyBuffer(b.dev, b_buf, NULL);
   vkFreeMemory(b.dev, b_mem, NULL);
   vkDestroyBuffer(b.dev, b_read, NULL);
   vkFreeMemory(b.dev, b_read_mem, NULL);
   vkDestroyBuffer(a.dev, a_buf, NULL);
   vkFreeMemory(a.dev, a_mem, NULL);
   vkDestroyBuffer(a.dev, a_stage, NULL);
   vkFreeMemory(a.dev, a_stage_mem, NULL);
   vkDestroyCommandPool(a.dev, a.pool, NULL);
   vkDestroyCommandPool(b.dev, b.pool, NULL);
   vkDestroyDevice(a.dev, NULL);
   vkDestroyDevice(b.dev, NULL);
   vkDestroyInstance(inst, NULL);

   if (bad) {
      fprintf(stderr, "FAIL: %u/%u values wrong\n", bad, n);
      return 1;
   }
   printf("PASS: %u/%u values correct through a dma-buf (device A export, device B import)\n", n, n);
   return 0;
}
