/*
 * helios_vk_present.c — Phase 7.0 go/no-go gate (DISPLAY.md §8).
 *
 * Proves (or disproves) that a venus-rendered, EXPORTABLE HOST3D blob can be
 * SET_SCANOUT_BLOB'd and displayed ZERO-COPY under `-spice gl=on`. This is the
 * single load-bearing unknown before the DOD rewrite: does the venus image's
 * backing VkDeviceMemory get exported by the host (ANV via the render-server) as
 * a real dmabuf, so QEMU's virgl_cmd_set_scanout_blob finds res->dmabuf_fd >= 0?
 *
 * What it does, all in one process (so the image stays alive across the present):
 *   1. Set HELIOS_GATE_RESID_FILE BEFORE loading vulkan-1.dll, so the Helios ICD
 *      (vn_renderer_helios.c gate hook) appends "<res_id> <size>" for every
 *      device-memory blob alloc.
 *   2. Create a 256x256 BGRA, LINEAR-tiled, EXPORTABLE (VkExternalMemoryImageCreateInfo
 *      + dedicated + VkExportMemoryAllocateInfo, DMA_BUF handle type) image — this is
 *      the proven venus scanout recipe (minus DRM modifiers, which venus does not
 *      advertise on a Windows guest). The vkAllocateMemory drives ALLOC_BLOB
 *      (HOST3D, blob_id=mem_id) in the ICD.
 *   3. Clear it to a bright solid color, submit, vkQueueWaitIdle (host GPU done).
 *   4. Read the gate file, pick the res_id whose size matches our memory size.
 *   5. Open the Helios device directly (SetupDi/CreateFile) and issue
 *      IOCTL_HELIOS_PRESENT_BLOB(res_id, 256, 256, BGRA, stride, offset) — the KMD
 *      does SET_SCANOUT_BLOB(scanout 0) + RESOURCE_FLUSH.
 *   6. If the IOCTL SUCCEEDS, the host accepted the scanout (dmabuf_fd >= 0): the
 *      gate's guest-side PASS. Keep the image alive and sleep so the SPICE/gtk
 *      window can be visually confirmed.
 *   A failing IOCTL (KMD maps a non-OK virtio response, e.g. RESP_ERR_UNSPEC) is
 *   the "resource not backed by a dmabuf" FAIL signal — the venus image was not
 *   created exportable / ANV did not export it.
 *
 * Build (mingw, on win11):
 *   gcc -O2 -o C:\Users\Rupansh\helios_vk_present.exe \
 *       Z:\icd\win-build\helios_vk_present.c -IZ:\icd\mesa\include -lsetupapi
 * Run (interactive session so the spice display is visible):
 *   $env:HELIOS_GATE_RESID_FILE="C:\Users\Rupansh\helios_gate_resid.txt"
 *   $env:VN_DEBUG="init"; .\helios_vk_present.exe
 */
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <setupapi.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define VK_NO_PROTOTYPES
#include <vulkan/vulkan.h>

/* ── Helios device interface + the throwaway present IOCTL (protocol/src/*) ──── */
static const GUID GUID_DEVINTERFACE_HELIOS = {
   0xC8F84237, 0xCD89, 0x48F5, { 0xAF, 0xC5, 0x32, 0x94, 0x45, 0x24, 0x62, 0x5C }
};
#define IOCTL_HELIOS_PRESENT_BLOB 0x0022E418u
#define HELIOS_ESCAPE_MAGIC       0x48454C53u /* 'HELS' */
#define HELIOS_ESCAPE_VERSION     1u
#define HELIOS_ESCAPE_PRESENT_BLOB 0x0007u
#define VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM 1u
#define DRM_FORMAT_MOD_LINEAR 0ULL

struct helios_escape_header {
   uint32_t magic, cmd_type, version, size;
};
struct helios_escape_present_blob {
   struct helios_escape_header hdr;
   uint32_t resource_id; /* in: venus blob resource to scan out */
   uint32_t width;       /* in: image width  */
   uint32_t height;      /* in: image height */
   uint32_t format;      /* in: VIRTIO_GPU_FORMAT_* */
   uint32_t stride;      /* in: plane-0 row pitch (bytes) */
   uint32_t offset;      /* in: plane-0 byte offset */
};
_Static_assert(sizeof(struct helios_escape_header) == 16, "hdr size");
_Static_assert(sizeof(struct helios_escape_present_blob) == 40, "present size");

#define ILOAD(name) (PFN_##name)(void *) gipa(inst, #name)
#define DLOAD(name) (PFN_##name)(void *) gdpa(dev, #name)

#define CHECK(expr)                                                            \
   do {                                                                        \
      VkResult _r = (expr);                                                    \
      fprintf(stderr, "  %-48s => %d %s\n", #expr, _r,                         \
              _r == VK_SUCCESS ? "OK" : "FAIL");                               \
      fflush(stderr);                                                          \
      if (_r != VK_SUCCESS)                                                    \
         return 10;                                                            \
   } while (0)

/* Scanout dimensions — env-overridable (HELIOS_W/HELIOS_H). Default 1024x768
 * (256x256 is below virt-viewer's 320px min display size). */
static uint32_t W = 1024u, H = 768u;

/* Open GUID_DEVINTERFACE_HELIOS (mirrors vn_renderer_helios.c::helios_open_device). */
static HANDLE
helios_open_device(void)
{
   HDEVINFO di = SetupDiGetClassDevsW(&GUID_DEVINTERFACE_HELIOS, NULL, NULL,
                                      DIGCF_PRESENT | DIGCF_DEVICEINTERFACE);
   if (di == INVALID_HANDLE_VALUE) {
      fprintf(stderr, "  SetupDiGetClassDevs failed: %lu\n", GetLastError());
      return INVALID_HANDLE_VALUE;
   }
   SP_DEVICE_INTERFACE_DATA ifd = { 0 };
   ifd.cbSize = sizeof(ifd);
   if (!SetupDiEnumDeviceInterfaces(di, NULL, &GUID_DEVINTERFACE_HELIOS, 0, &ifd)) {
      fprintf(stderr, "  no Helios interface present: %lu\n", GetLastError());
      SetupDiDestroyDeviceInfoList(di);
      return INVALID_HANDLE_VALUE;
   }
   DWORD need = 0;
   SetupDiGetDeviceInterfaceDetailW(di, &ifd, NULL, 0, &need, NULL);
   SP_DEVICE_INTERFACE_DETAIL_DATA_W *detail = calloc(1, need);
   HANDLE h = INVALID_HANDLE_VALUE;
   if (detail) {
      detail->cbSize = sizeof(*detail);
      if (SetupDiGetDeviceInterfaceDetailW(di, &ifd, detail, need, NULL, NULL)) {
         h = CreateFileW(detail->DevicePath, GENERIC_READ | GENERIC_WRITE,
                         FILE_SHARE_READ | FILE_SHARE_WRITE, NULL, OPEN_EXISTING, 0, NULL);
         if (h == INVALID_HANDLE_VALUE)
            fprintf(stderr, "  CreateFile(Helios) failed: %lu\n", GetLastError());
      }
      free(detail);
   }
   SetupDiDestroyDeviceInfoList(di);
   return h;
}

/* Scan the gate file for the last "<res_id> <size>" line whose size matches. */
static uint32_t
read_resid_for_size(const char *path, uint64_t want_size)
{
   FILE *f = fopen(path, "r");
   if (!f) {
      fprintf(stderr, "  gate file '%s' not found (ICD wrote nothing?)\n", path);
      return 0;
   }
   uint32_t res = 0, rid;
   unsigned long long sz;
   while (fscanf(f, "%u %llu", &rid, &sz) == 2) {
      fprintf(stderr, "    gate entry: res_id=%u size=%llu\n", rid, sz);
      if (sz == want_size)
         res = rid; /* keep the last match */
   }
   fclose(f);
   return res;
}

int
main(int argc, char **argv)
{
   /* (1) Point the ICD gate hook at a file BEFORE loading the ICD, so its CRT
    * snapshots the env var. Clear any stale file first. */
   const char *gate_file = (argc > 1) ? argv[1]
      : (getenv("HELIOS_GATE_RESID_FILE") ? getenv("HELIOS_GATE_RESID_FILE")
                                          : "C:\\Users\\Rupansh\\helios_gate_resid.txt");
   SetEnvironmentVariableA("HELIOS_GATE_RESID_FILE", gate_file);
   _putenv_s("HELIOS_GATE_RESID_FILE", gate_file);
   DeleteFileA(gate_file);
   fprintf(stderr, "gate res_id file: %s\n", gate_file);
   if (getenv("HELIOS_W")) W = (uint32_t)atoi(getenv("HELIOS_W"));
   if (getenv("HELIOS_H")) H = (uint32_t)atoi(getenv("HELIOS_H"));
   fprintf(stderr, "scanout size: %ux%u\n", W, H);

   HMODULE vk = LoadLibraryW(L"vulkan-1.dll");
   if (!vk) { fprintf(stderr, "no vulkan-1.dll\n"); return 1; }
   PFN_vkGetInstanceProcAddr gipa =
      (PFN_vkGetInstanceProcAddr)(void *)GetProcAddress(vk, "vkGetInstanceProcAddr");
   PFN_vkCreateInstance pCreateInstance =
      (PFN_vkCreateInstance)gipa(NULL, "vkCreateInstance");

   const VkApplicationInfo app = {
      .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .pApplicationName = "helios_vk_present",
      .apiVersion = VK_API_VERSION_1_1,
   };
   const VkInstanceCreateInfo ici = {
      .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
      .pApplicationInfo = &app,
   };
   VkInstance inst = VK_NULL_HANDLE;
   CHECK(pCreateInstance(&ici, NULL, &inst));

   PFN_vkEnumeratePhysicalDevices pEnum = ILOAD(vkEnumeratePhysicalDevices);
   PFN_vkGetPhysicalDeviceProperties pProps = ILOAD(vkGetPhysicalDeviceProperties);
   PFN_vkGetPhysicalDeviceQueueFamilyProperties pQFP =
      ILOAD(vkGetPhysicalDeviceQueueFamilyProperties);
   PFN_vkGetPhysicalDeviceMemoryProperties pMemProps =
      ILOAD(vkGetPhysicalDeviceMemoryProperties);
   PFN_vkEnumerateDeviceExtensionProperties pEnumExt =
      ILOAD(vkEnumerateDeviceExtensionProperties);
   PFN_vkGetPhysicalDeviceImageFormatProperties2 pImgFmt2 =
      ILOAD(vkGetPhysicalDeviceImageFormatProperties2);
   PFN_vkCreateDevice pCreateDevice = ILOAD(vkCreateDevice);
   PFN_vkGetDeviceProcAddr gdpa = ILOAD(vkGetDeviceProcAddr);

   uint32_t count = 0;
   CHECK(pEnum(inst, &count, NULL));
   VkPhysicalDevice devs[8];
   if (count > 8) count = 8;
   CHECK(pEnum(inst, &count, devs));
   /* Pick the GPU whose name contains $HELIOS_GPU (default "Intel" — ANV is the
    * known-good dma-buf scanout exporter; NVIDIA host-visible export scans out
    * black here). Falls back to device 0. */
   const char *want_gpu = getenv("HELIOS_GPU");
   if (!want_gpu) want_gpu = "Intel";
   VkPhysicalDevice phys = devs[0];
   for (uint32_t i = 0; i < count; i++) {
      VkPhysicalDeviceProperties pp;
      pProps(devs[i], &pp);
      fprintf(stderr, "  device[%u]: %s\n", i, pp.deviceName);
      if (strstr(pp.deviceName, want_gpu)) phys = devs[i];
   }
   VkPhysicalDeviceProperties props;
   pProps(phys, &props);
   fprintf(stderr, "Using device: \"%s\" (match='%s')\n", props.deviceName, want_gpu);

   /* List external-memory-relevant device extensions (diagnostic). */
   uint32_t extn = 0;
   pEnumExt(phys, NULL, &extn, NULL);
   VkExtensionProperties *exts = calloc(extn, sizeof(*exts));
   pEnumExt(phys, NULL, &extn, exts);
   const char *wanted[] = {
      "VK_KHR_external_memory", "VK_KHR_external_memory_fd",
      "VK_EXT_external_memory_dma_buf", "VK_KHR_external_memory_win32",
      "VK_EXT_external_memory_host", "VK_EXT_image_drm_format_modifier",
   };
   const char *enable[8];
   uint32_t enable_n = 0;
   fprintf(stderr, "external-memory device extensions present:\n");
   for (uint32_t w = 0; w < sizeof(wanted) / sizeof(wanted[0]); w++) {
      int found = 0;
      for (uint32_t i = 0; i < extn; i++)
         if (!strcmp(exts[i].extensionName, wanted[w])) { found = 1; break; }
      fprintf(stderr, "    %-36s %s\n", wanted[w], found ? "YES" : "no");
      if (found && enable_n < 8)
         enable[enable_n++] = wanted[w]; /* enable all present, incl. drm_format_modifier */
   }
   free(exts);

   uint32_t qfcount = 0;
   pQFP(phys, &qfcount, NULL);
   VkQueueFamilyProperties qfs[16];
   if (qfcount > 16) qfcount = 16;
   pQFP(phys, &qfcount, qfs);
   uint32_t qfi = 0;
   for (uint32_t i = 0; i < qfcount; i++)
      if (qfs[i].queueFlags & (VK_QUEUE_GRAPHICS_BIT | VK_QUEUE_COMPUTE_BIT)) {
         qfi = i;
         break;
      }

   /* Phase 7: a real DRM_FORMAT_MODIFIER (LINEAR) DMA_BUF image. venus REJECTS a
    * DMA_BUF scanout image whose tiling isn't TILING_DRM_FORMAT_MODIFIER_EXT, and
    * the exported dmabuf needs an explicit modifier for the host to interpret it
    * (a plain LINEAR image -> MOD_INVALID -> black). Requires the ICD gate removal
    * (VK_EXT_image_drm_format_modifier + VK_EXT_external_memory_dma_buf on Win). */
   const VkExternalMemoryHandleTypeFlagBits htype =
      VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT;
   const VkImageUsageFlags usage = VK_IMAGE_USAGE_TRANSFER_DST_BIT;
   const uint64_t modifier = DRM_FORMAT_MOD_LINEAR;
   {
      VkPhysicalDeviceImageDrmFormatModifierInfoEXT mi = {
         .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_DRM_FORMAT_MODIFIER_INFO_EXT,
         .drmFormatModifier = modifier,
         .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
      };
      VkPhysicalDeviceExternalImageFormatInfo ext = {
         .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_IMAGE_FORMAT_INFO,
         .pNext = &mi, .handleType = htype,
      };
      VkPhysicalDeviceImageFormatInfo2 fi = {
         .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2,
         .pNext = &ext, .format = VK_FORMAT_B8G8R8A8_UNORM, .type = VK_IMAGE_TYPE_2D,
         .tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT, .usage = usage, .flags = 0,
      };
      VkExternalImageFormatProperties efp = { .sType = VK_STRUCTURE_TYPE_EXTERNAL_IMAGE_FORMAT_PROPERTIES };
      VkImageFormatProperties2 p2 = { .sType = VK_STRUCTURE_TYPE_IMAGE_FORMAT_PROPERTIES_2, .pNext = &efp };
      VkResult r = pImgFmt2(phys, &fi, &p2);
      fprintf(stderr, "  modifier-LINEAR DMA_BUF imgfmt2 => r=%d features=0x%x %s\n",
              r, efp.externalMemoryProperties.externalMemoryFeatures,
              (r == VK_SUCCESS) ? "OK" : "(unsupported — host is the final judge)");
   }
   fprintf(stderr, "  chosen: handleType=DMA_BUF usage=0x%x tiling=DRM_FORMAT_MODIFIER(LINEAR)\n", usage);

   const float prio = 1.0f;
   const VkDeviceQueueCreateInfo qci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = qfi,
      .queueCount = 1,
      .pQueuePriorities = &prio,
   };
   const VkDeviceCreateInfo dci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
      .queueCreateInfoCount = 1,
      .pQueueCreateInfos = &qci,
      .enabledExtensionCount = enable_n,
      .ppEnabledExtensionNames = enable,
   };
   VkDevice dev = VK_NULL_HANDLE;
   CHECK(pCreateDevice(phys, &dci, NULL, &dev));

   PFN_vkGetDeviceQueue pGetQueue = DLOAD(vkGetDeviceQueue);
   PFN_vkCreateImage pCreateImage = DLOAD(vkCreateImage);
   PFN_vkGetImageMemoryRequirements pImgReq = DLOAD(vkGetImageMemoryRequirements);
   PFN_vkAllocateMemory pAlloc = DLOAD(vkAllocateMemory);
   PFN_vkBindImageMemory pBindImg = DLOAD(vkBindImageMemory);
   PFN_vkGetImageSubresourceLayout pSubLayout = DLOAD(vkGetImageSubresourceLayout);
   PFN_vkCreateCommandPool pCreatePool = DLOAD(vkCreateCommandPool);
   PFN_vkAllocateCommandBuffers pAllocCmd = DLOAD(vkAllocateCommandBuffers);
   PFN_vkBeginCommandBuffer pBegin = DLOAD(vkBeginCommandBuffer);
   PFN_vkCmdPipelineBarrier pBarrier = DLOAD(vkCmdPipelineBarrier);
   PFN_vkCmdClearColorImage pClear = DLOAD(vkCmdClearColorImage);
   PFN_vkEndCommandBuffer pEnd = DLOAD(vkEndCommandBuffer);
   PFN_vkQueueSubmit pSubmit = DLOAD(vkQueueSubmit);
   PFN_vkCreateFence pCreateFence = DLOAD(vkCreateFence);
   PFN_vkWaitForFences pWait = DLOAD(vkWaitForFences);
   PFN_vkQueueWaitIdle pQWaitIdle = DLOAD(vkQueueWaitIdle);

   VkQueue queue = VK_NULL_HANDLE;
   pGetQueue(dev, qfi, 0, &queue);

   /* (2) Exportable DRM_FORMAT_MODIFIER (LINEAR) BGRA image. */
   uint64_t mods[1] = { modifier };
   VkImageDrmFormatModifierListCreateInfoEXT modList = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_LIST_CREATE_INFO_EXT,
      .drmFormatModifierCount = 1,
      .pDrmFormatModifiers = mods,
   };
   VkExternalMemoryImageCreateInfo extImg = {
      .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
      .pNext = &modList,
      .handleTypes = htype,
   };
   VkImageCreateInfo ic = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
      .pNext = &extImg,
      .imageType = VK_IMAGE_TYPE_2D,
      .format = VK_FORMAT_B8G8R8A8_UNORM,
      .extent = { W, H, 1 },
      .mipLevels = 1,
      .arrayLayers = 1,
      .samples = VK_SAMPLE_COUNT_1_BIT,
      .tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT,
      .usage = usage,
      .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
      .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
   };
   VkImage image = VK_NULL_HANDLE;
   CHECK(pCreateImage(dev, &ic, NULL, &image));

   VkMemoryRequirements req;
   pImgReq(dev, image, &req);
   VkPhysicalDeviceMemoryProperties mp;
   pMemProps(phys, &mp);
   uint32_t mti = UINT32_MAX;
   /* Prefer BOTH DEVICE_LOCAL and HOST_VISIBLE (Intel UMA): export-able as a
    * scanout dmabuf AND CPU-writable so we can put authoritative bytes in and
    * read them back. Fall back to HOST_VISIBLE, then DEVICE_LOCAL, then any. */
   const VkMemoryPropertyFlags both =
      VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT | VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT;
   for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
      if ((req.memoryTypeBits & (1u << i)) &&
          (mp.memoryTypes[i].propertyFlags & both) == both) { mti = i; break; }
   if (mti == UINT32_MAX)
      for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
         if ((req.memoryTypeBits & (1u << i)) &&
             (mp.memoryTypes[i].propertyFlags & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT)) { mti = i; break; }
   if (mti == UINT32_MAX)
      for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
         if ((req.memoryTypeBits & (1u << i)) &&
             (mp.memoryTypes[i].propertyFlags & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT)) { mti = i; break; }
   if (mti == UINT32_MAX)
      for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
         if (req.memoryTypeBits & (1u << i)) { mti = i; break; }
   if (mti == UINT32_MAX) { fprintf(stderr, "FAIL: no memory type\n"); return 5; }
   const VkMemoryPropertyFlags mflags = mp.memoryTypes[mti].propertyFlags;
   fprintf(stderr, "  image mem: size=%llu typeIndex=%u flags=0x%x (DEVICE_LOCAL=%d HOST_VISIBLE=%d COHERENT=%d)\n",
           (unsigned long long)req.size, mti, mflags,
           !!(mflags & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT),
           !!(mflags & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT),
           !!(mflags & VK_MEMORY_PROPERTY_HOST_COHERENT_BIT));

   VkExportMemoryAllocateInfo expMem = {
      .sType = VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO,
      .handleTypes = htype,
   };
   VkMemoryDedicatedAllocateInfo dedi = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
      .pNext = &expMem,
      .image = image,
   };
   VkMemoryAllocateInfo mai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .pNext = &dedi,
      .allocationSize = req.size,
      .memoryTypeIndex = mti,
   };
   VkDeviceMemory mem = VK_NULL_HANDLE;
   CHECK(pAlloc(dev, &mai, NULL, &mem)); /* drives ALLOC_BLOB(HOST3D, blob_id=mem_id) */
   CHECK(pBindImg(dev, image, mem, 0));

   /* Plane-0 geometry for SET_SCANOUT_BLOB. DRM_FORMAT_MODIFIER images report
    * layout per MEMORY_PLANE aspect (not COLOR). */
   VkImageSubresource sub = { VK_IMAGE_ASPECT_MEMORY_PLANE_0_BIT_EXT, 0, 0 };
   VkSubresourceLayout lay;
   pSubLayout(dev, image, &sub, &lay);
   uint32_t stride = (uint32_t)lay.rowPitch;
   uint32_t offset = (uint32_t)lay.offset;
   fprintf(stderr, "  subresource: rowPitch=%llu offset=%llu\n",
           (unsigned long long)lay.rowPitch, (unsigned long long)lay.offset);

   /* (3) Clear to solid magenta, submit, wait. */
   VkCommandPool pool = VK_NULL_HANDLE;
   const VkCommandPoolCreateInfo pci = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
      .queueFamilyIndex = qfi,
   };
   CHECK(pCreatePool(dev, &pci, NULL, &pool));
   VkCommandBuffer cmd = VK_NULL_HANDLE;
   const VkCommandBufferAllocateInfo cbai = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
      .commandPool = pool,
      .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
      .commandBufferCount = 1,
   };
   CHECK(pAllocCmd(dev, &cbai, &cmd));

   const VkImageSubresourceRange range = {
      VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1,
   };
   const VkCommandBufferBeginInfo cbi = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
      .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
   };
   CHECK(pBegin(cmd, &cbi));
   VkImageMemoryBarrier toGeneral = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
      .srcAccessMask = 0,
      .dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
      .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
      .newLayout = VK_IMAGE_LAYOUT_GENERAL,
      .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
      .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
      .image = image,
      .subresourceRange = range,
   };
   pBarrier(cmd, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT,
            0, 0, NULL, 0, NULL, 1, &toGeneral);
   /* Bright magenta so a partial/garbled scanout is obvious vs. a clean frame. */
   const VkClearColorValue color = { .float32 = { 1.0f, 0.0f, 1.0f, 1.0f } };
   pClear(cmd, image, VK_IMAGE_LAYOUT_GENERAL, &color, 1, &range);
   /* Release ownership to the FOREIGN (host scanout) queue so the GPU writes are
    * flushed and the exported dmabuf reflects the cleared content. */
   VkImageMemoryBarrier flush = toGeneral;
   flush.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
   flush.dstAccessMask = 0;
   flush.oldLayout = VK_IMAGE_LAYOUT_GENERAL;
   flush.newLayout = VK_IMAGE_LAYOUT_GENERAL;
   flush.srcQueueFamilyIndex = qfi;
   flush.dstQueueFamilyIndex = VK_QUEUE_FAMILY_FOREIGN_EXT;
   pBarrier(cmd, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT,
            0, 0, NULL, 0, NULL, 1, &flush);
   CHECK(pEnd(cmd));

   VkFence fence = VK_NULL_HANDLE;
   const VkFenceCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
   CHECK(pCreateFence(dev, &fci, NULL, &fence));
   const VkSubmitInfo si = {
      .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
      .commandBufferCount = 1,
      .pCommandBuffers = &cmd,
   };
   fprintf(stderr, "[X] vkQueueSubmit (clear magenta) ...\n"); fflush(stderr);
   CHECK(pSubmit(queue, 1, &si, fence));
   CHECK(pWait(dev, 1, &fence, VK_TRUE, 5000000000ull));
   {
      VkResult wi = pQWaitIdle(queue);
      fprintf(stderr, "  vkQueueWaitIdle => %d %s\n", wi, wi == VK_SUCCESS ? "OK" : "ERR");
      fflush(stderr);
   }

   /* (3b) Authoritative content: if HOST_VISIBLE, CPU-write solid magenta straight
    * into the blob memory (the exact bytes the host dmabuf must reference) and read
    * one back. Removes GPU-clear<->dmabuf coherency from the question: if the host
    * STILL shows black after this, the exported dmabuf is a DIFFERENT buffer than
    * this memory (venus export not honored) — the decisive diagnosis. */
   if (mflags & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT) {
      PFN_vkMapMemory pMap = DLOAD(vkMapMemory);
      PFN_vkUnmapMemory pUnmap = DLOAD(vkUnmapMemory);
      PFN_vkFlushMappedMemoryRanges pFlush = DLOAD(vkFlushMappedMemoryRanges);
      void *p = NULL;
      VkResult mr = pMap(dev, mem, 0, VK_WHOLE_SIZE, 0, &p);
      fprintf(stderr, "  vkMapMemory => %d %s\n", mr, mr == VK_SUCCESS ? "OK" : "FAIL");
      if (mr == VK_SUCCESS && p) {
         for (uint32_t y = 0; y < H; y++) {
            uint32_t *row = (uint32_t *)((uint8_t *)p + offset + (size_t)y * stride);
            for (uint32_t x = 0; x < W; x++) row[x] = 0xFFFF00FFu; /* BGRA magenta */
         }
         if (!(mflags & VK_MEMORY_PROPERTY_HOST_COHERENT_BIT)) {
            VkMappedMemoryRange r = { .sType = VK_STRUCTURE_TYPE_MAPPED_MEMORY_RANGE,
                                      .memory = mem, .offset = 0, .size = VK_WHOLE_SIZE };
            pFlush(dev, 1, &r);
         }
         uint32_t back = ((uint32_t *)((uint8_t *)p + offset))[0];
         fprintf(stderr, "  CPU-filled magenta; readback px[0]=0x%08x %s\n",
                 back, back == 0xFFFF00FFu ? "(magenta OK)" : "(MISMATCH!)");
         pUnmap(dev, mem);
      }
   } else {
      fprintf(stderr, "  (memory not HOST_VISIBLE — relying on the GPU clear only)\n");
   }

   /* (4) Learn the res_id of this image's backing blob (match by size). */
   uint32_t res_id = read_resid_for_size(gate_file, (uint64_t)req.size);
   if (!res_id) {
      fprintf(stderr, "FAIL: no res_id found for size %llu — did the ICD gate hook "
              "fire? (HELIOS_GATE_RESID_FILE set before LoadLibrary?)\n",
              (unsigned long long)req.size);
      return 7;
   }
   fprintf(stderr, "  ==> presenting blob res_id=%u (%ux%u BGRA stride=%u offset=%u)\n",
           res_id, W, H, stride, offset);

   /* (5) Issue the throwaway SET_SCANOUT_BLOB present IOCTL. */
   HANDLE hdev = helios_open_device();
   if (hdev == INVALID_HANDLE_VALUE) { fprintf(stderr, "FAIL: open Helios device\n"); return 8; }
   struct helios_escape_present_blob pb = {
      .hdr = { HELIOS_ESCAPE_MAGIC, HELIOS_ESCAPE_PRESENT_BLOB, HELIOS_ESCAPE_VERSION,
               (uint32_t)sizeof(pb) },
      .resource_id = res_id,
      .width = W,
      .height = H,
      .format = VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM,
      .stride = stride,
      .offset = offset,
   };
   DWORD returned = 0;
   BOOL ok = DeviceIoControl(hdev, IOCTL_HELIOS_PRESENT_BLOB, &pb, sizeof(pb), &pb,
                             sizeof(pb), &returned, NULL);
   DWORD err = GetLastError();
   CloseHandle(hdev);

   if (!ok) {
      fprintf(stderr,
              "FAIL: IOCTL_HELIOS_PRESENT_BLOB Win32 error %lu — the host REJECTED the "
              "scanout (likely RESP_ERR_UNSPEC: blob not backed by a dmabuf). The venus "
              "image did not export. GATE: NO-GO (fix venus export / check host log).\n",
              err);
      return 9;
   }

   fprintf(stderr,
           "PASS (guest side): SET_SCANOUT_BLOB + RESOURCE_FLUSH accepted by the host. "
           "The venus blob IS dmabuf-backed. >>> LOOK AT THE SPICE/GTK DISPLAY: a solid "
           "MAGENTA frame == zero-copy venus scanout works. GATE: GO. <<<\n");
   /* Hold the image (and thus the scanned-out blob) alive for visual confirmation.
    * Headless pre-check: set HELIOS_HOLD_SECS=2 (nothing to see over session-0 SSH). */
   int hold = getenv("HELIOS_HOLD_SECS") ? atoi(getenv("HELIOS_HOLD_SECS")) : 90;
   fprintf(stderr, "Holding the image alive for %ds for visual confirmation...\n", hold);
   fflush(stderr);
   Sleep((DWORD)hold * 1000);
   return 0;
}
